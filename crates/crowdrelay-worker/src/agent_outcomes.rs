//! Agent outcome ingestion worker.
//!
//! Polls the `agent_outcomes` handoff table for rows written by the
//! `crowdrelay-agents` TypeScript service, validates each payload against the
//! versioned Rust mirror of the zod schemas, and maps the outcome into
//! autopilot decision (+ action, for `require_approval` kinds) rows.
//!
//! Ownership: the agents service is the ONLY writer of `agent_outcomes`; this
//! worker is the only reader/mapper. `agent_fan_segments` is written here too
//! — single-writer per table.
//!
//! `agent_outreach_targets` has two writers, split by where the target came
//! from: this path writes what an agent proposed, and
//! `audience_graph::community_promotion` writes what discovery already found
//! and the screening policy admitted. Both go through
//! `screen_community_candidate`. Personal-contact kinds dedupe on
//! `(workspace_id, display_name, target_kind)`; a community's identity is
//! its subreddit instead, so both writers conflict on the normalized
//! `normalize_subreddit(subreddit)` index — whichever sees a community first
//! wins the row and the other updates it.
//!
//! Idempotency: `agent_outcomes.idempotency_key` is unique per
//! (workspace_id, key), and the autopilot decision_key mirrors it, so worker
//! retries and task re-runs can never double-create decisions.

use crate::auto_post_platforms::AutoPostPlatforms;
use crate::community_vetting::{community_place, community_snapshot};
use std::time::Duration;

use crowdrelay_application::agent_outcomes::{
    OutcomeKind, ProvenanceRejection, ValidatedOutcome, evidence_confidence_basis_points,
    provenance_admission, validate,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::target_discovery::{
    ScreeningVerdict, TargetDiscoveryPolicy, screen_community_candidate,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

include!("agent_outcomes/press_recipient.rs");

const BATCH_LIMIT: i64 = 32;

/// The `agent_outreach_targets_target_kind_check` vocabulary (migration 0138).
///
/// Deliberately not `OutreachTargetKind`. That enum is the vocabulary of
/// `viryaos_outreach_targets`, a different table in a different bounded
/// context, and the two sets genuinely differ: this one accepts `community`
/// and rejects `support_slot`, which is the reverse of the other. Reaching for
/// the enum because the column names match would accept `support_slot` here
/// and hand it straight to the CHECK that forbids it.
const AGENT_TARGET_KINDS: [&str; 7] = [
    "press",
    "radio",
    "playlist",
    "media_patronage",
    "endorsement",
    "creator",
    "community",
];

#[derive(Debug, Error)]
pub enum AgentOutcomeError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("validation error: {0}")]
    Validation(#[from] crowdrelay_application::agent_outcomes::OutcomeValidationError),
    #[error("agent proposed target_kind {0:?}, which agent_outreach_targets does not accept")]
    UnknownTargetKind(String),
    #[error(
        "no press target has a contact email, so a press pitch has nobody to reach — \
         add a contact_email to an agent_outreach_targets row with target_kind='press'"
    )]
    NoPressRecipient,
    #[error("agent outcome payload does not match the action schema")]
    UnpersistablePayload,
    /// The quality guard runs before the transaction opens, so a rejection
    /// surfacing here means the finding-shaped write re-checked its own
    /// contract rather than trusting the caller. The process loop records it
    /// as a rejection either way.
    #[error("{0}")]
    Rejected(#[from] OutcomeRejection),
}

include!("agent_outcomes/rejections.rs");

include!("agent_outcomes/quality_guard.rs");
include!("agent_outcomes/opportunity_findings.rs");
include!("agent_outcomes/creative_family.rs");
include!("agent_outcomes/community_engagement.rs");

/// True for a relative in-app route the Signal app can resolve.
///
/// Rejects anything carrying a scheme or an authority — `https://`,
/// `javascript:`, `//evil.example` — and anything not anchored at the root.
/// Deliberately a shape test: whether `/events/{id}` names a row that exists
/// is a database question, and this guard is pure so it stays unit-testable.
fn is_in_app_route(target: &str) -> bool {
    target.starts_with('/') && !target.starts_with("//") && !target.contains("://")
}

#[derive(Clone, Debug)]
pub struct AgentOutcomeWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
    /// The origins an agent-created smart link may redirect to.
    ///
    /// A smart link is a redirect on the tenant's own domain and an agent's
    /// destination comes out of a language model, so without this the domain
    /// is an open redirect whose target a model picks. See
    /// `domain::acquisition::agent_smart_link_destination`.
    public_origin: String,
    /// Channels the operator already granted standing approval to, by setting
    /// that channel's auto-post flag. See `auto_post_platforms`.
    auto_post_platforms: AutoPostPlatforms,
}

impl AgentOutcomeWorker {
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        operation_timeout: Duration,
        public_origin: String,
        auto_post_platforms: AutoPostPlatforms,
    ) -> Self {
        Self {
            pool,
            workspace_id,
            poll_interval,
            operation_timeout,
            public_origin,
            auto_post_platforms,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut ticks = interval(self.poll_interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                _ = ticks.tick() => {
                    match timeout(self.operation_timeout, self.run_once()).await {
                        Ok(Ok(processed)) if processed > 0 => {
                            tracing::info!(processed, "agent outcome worker processed batch");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "agent outcome worker cycle failed"),
                        Err(_) => tracing::warn!("agent outcome worker cycle timed out"),
                    }
                }
            }
        }
    }

    /// Public so the postgres suite can drive one ingestion cycle and assert
    /// what the decision it wrote is correlated to. The trace resolution is the
    /// product claim here -- an agent-produced decision must join to the action
    /// that caused it -- and it is only observable through a real cycle.
    pub async fn run_once(&self) -> Result<usize, AgentOutcomeError> {
        // Recover any outcomes stuck in 'processing' from a previous crash.
        // The poll query only selects 'pending', so without this a crash
        // between the UPDATE to 'processing' and the commit leaves rows
        // stranded forever.
        self.recover_stale_processing().await?;

        let mut total = 0;
        loop {
            let processed = self.process_batch().await?;
            if processed == 0 {
                break;
            }
            total += processed;
        }
        Ok(total)
    }

    /// Resets outcomes stuck in 'processing' for more than 10 minutes back to
    /// 'pending' so they can be retried. This handles worker crashes between
    /// the claim (`SET status = 'processing'`) and the commit. Uses
    /// `created_at` because the table has no `updated_at` column — if a row
    /// was created more than 10 minutes ago and is still processing, the
    /// worker that claimed it is dead.
    async fn recover_stale_processing(&self) -> Result<(), AgentOutcomeError> {
        let reset = sqlx::query(
            r#"
            UPDATE agent_outcomes
            SET status = 'pending'
            WHERE workspace_id = $1
              AND status = 'processing'
              AND created_at < now() - INTERVAL '10 minutes'
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .execute(&self.pool)
        .await?;
        if reset.rows_affected() > 0 {
            tracing::info!(
                recovered = reset.rows_affected(),
                "recovered stale processing agent outcomes"
            );
        }
        Ok(())
    }

    /// Claims one batch of pending outcomes (FOR UPDATE SKIP LOCKED), validates
    /// each, maps to autopilot rows, and marks the outcome processed or
    /// rejected. Each outcome is its own transaction so one bad payload cannot
    /// roll back a whole batch.
    async fn process_batch(&self) -> Result<usize, AgentOutcomeError> {
        let rows = sqlx::query_as::<_, OutcomeRow>(
            r#"
            UPDATE agent_outcomes
            SET status = 'processing'
            WHERE id IN (
                SELECT id FROM agent_outcomes
                WHERE workspace_id = $1 AND status = 'pending'
                ORDER BY created_at
                LIMIT $2
                FOR UPDATE SKIP LOCKED
            )
            RETURNING id, workspace_id, task_id, result_id, kind, schema_version,
                      payload, confidence_basis_points, idempotency_key, trace_id
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(BATCH_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let mut processed = 0;
        for row in rows {
            let outcome = match validate(
                row.id,
                row.workspace_id,
                row.task_id,
                row.result_id,
                &row.kind,
                row.schema_version,
                &row.payload,
                row.confidence_basis_points,
                row.idempotency_key.clone(),
                row.trace_id,
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::warn!(
                        outcome_id = %row.id,
                        error = %error,
                        "rejecting agent outcome"
                    );
                    self.reject_outcome(row.id, &error.to_string()).await?;
                    continue;
                }
            };

            match self.map_outcome(&outcome).await {
                Ok(_) => {
                    processed += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        outcome_id = %outcome.id,
                        error = %error,
                        "failed to map agent outcome"
                    );
                    self.reject_outcome(outcome.id, &error.to_string()).await?;
                }
            }
        }
        Ok(processed)
    }

    /// The trace this outcome belongs to. Never `None`.
    ///
    /// An agent-produced decision used to be an orphan in the audit timeline.
    /// The agents service is the only writer of `agent_outcomes` and has never
    /// populated `trace_id` — 67 of 67 rows in production carried NULL — so
    /// every decision mapped from one was recorded with no correlation, and the
    /// question the ledger exists to answer ("what caused this?") had no answer
    /// for exactly the non-deterministic half of the system.
    ///
    /// The correlation was never actually lost, only unrecorded: the outcome
    /// names its task, and the task's metadata names the action that dispatched
    /// it. So resolve in order of directness and stop at the first answer:
    ///
    /// 1. what the outcome itself declares, if the agents service ever starts
    ///    sending it;
    /// 2. the trace stamped into the task's metadata at dispatch;
    /// 3. the trace of the action named by that metadata, which repairs every
    ///    task dispatched before the stamp existed;
    /// 4. the task's own id, as a trace root.
    ///
    /// Step 4 is not a fabricated correlation. It says "this outcome's history
    /// starts at its task", which is true of anything the agents service
    /// scheduled on its own rather than on an action's behalf, and it keeps the
    /// invariant total: no decision without a trace.
    ///
    /// Two deliberate choices about failure:
    ///
    /// `agent_service_tasks` belongs to the agents service, not to this
    /// repository — it is in `FOREIGN_RELATIONS` and no migration here creates
    /// it. A deployment without the agents schema must still ingest outcomes,
    /// so a lookup failure falls back to step 4 rather than propagating.
    ///
    /// And it runs on the pool rather than inside the caller's transaction. A
    /// statement that errors inside a transaction poisons it: querying a table
    /// that does not exist would abort the whole mapping, so enriching the trace
    /// would have been able to stop the ledger from being written at all.
    async fn resolve_trace(&self, outcome: &ValidatedOutcome) -> Uuid {
        if let Some(trace_id) = outcome.trace_id {
            return trace_id;
        }
        let resolved = sqlx::query_scalar::<_, Option<Uuid>>(
            r#"
            SELECT COALESCE(
                       (task.metadata ->> 'trace_id')::uuid,
                       action.trace_id,
                       task.id
                   )
            FROM agent_service_tasks AS task
            LEFT JOIN viryaos_autopilot_actions AS action
                   ON action.workspace_id = task.workspace_id
                  AND action.id = (task.metadata ->> 'action_id')::uuid
            WHERE task.workspace_id = $1 AND task.id = $2
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(outcome.task_id)
        .fetch_optional(&self.pool)
        .await;
        match resolved {
            Ok(found) => found.flatten().unwrap_or(outcome.task_id),
            Err(error) => {
                tracing::debug!(
                    outcome_id = %outcome.id,
                    error = %error,
                    "could not resolve the dispatching trace; rooting at the task"
                );
                outcome.task_id
            }
        }
    }

    /// Maps a validated outcome into autopilot decision (+ action) rows and
    /// any side tables (fan_segments, outreach_targets) in one transaction.
    async fn map_outcome(
        &self,
        outcome: &ValidatedOutcome,
    ) -> Result<(Option<Uuid>, Option<Uuid>), AgentOutcomeError> {
        // Hard data-quality guard: NO EVIDENCE = NO OPPORTUNITY.
        // A connector failure (Reddit credential error) produces 0
        // confidence, 0 evidence, and "Unnamed target". Without this guard
        // that still became a decision with an awaiting_approval action.
        // The guard rejects before any decision or action row is created.
        if let Err(rejection) = evaluate_outcome_quality(outcome) {
            tracing::warn!(
                outcome_id = %outcome.id,
                kind = %outcome.kind.as_str(),
                self_reported_confidence =
                    outcome.self_reported_confidence.self_reported_basis_points(),
                rejection = %rejection,
                "rejecting outcome: data-quality guard — no decision or action created"
            );
            self.reject_outcome(outcome.id, &rejection.to_string())
                .await?;
            return Ok((None, None));
        }

        // Resolved before the transaction opens: the lookup touches a table the
        // agents service owns, and a failed statement inside a transaction
        // would abort the mapping it is only meant to annotate.
        let trace_id = self.resolve_trace(outcome).await;
        // Which template produced this task decides what a community post may
        // source its facts from: the engager shares release videos, the repost
        // worker relays the band's own synced social posts. Resolved on the
        // pool for the same reason the trace is — `agent_service_tasks` is a
        // foreign schema. A failed lookup falls back to `video`, the strictest
        // existing gate, so a missing table cannot widen what may be posted.
        let producing_template: Option<String> = sqlx::query_scalar(
            r#"
            SELECT template_id FROM agent_service_tasks
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(outcome.task_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap_or_else(|error| {
            tracing::debug!(
                outcome_id = %outcome.id,
                error = %error,
                "could not resolve the producing template; strictest source gate applies"
            );
            None
        });
        let mut tx = self.pool.begin().await?;
        let decision_id = Uuid::now_v7();

        // A scout finding names its own subject: the decision records what
        // the brain made of the finding, and the shortlist joins that say to
        // the row it is about. The opportunity lands first so the decision
        // can point at it.
        let (subject_kind, subject_id) = match outcome.kind {
            OutcomeKind::OpportunityFindings => match &outcome.payload.item {
                Some(item) => (
                    "team_opportunity",
                    insert_opportunity_finding(&mut tx, outcome, item).await?,
                ),
                // The quality guard refuses a finding without an item before
                // the transaction opens; this arm exists only so the subject
                // pair is total.
                None => ("agent_outcome", outcome.id),
            },
            _ => ("agent_outcome", outcome.id),
        };

        let input_snapshot = json!({
            "task_id": outcome.task_id,
            "result_id": outcome.result_id,
            "schema_version": outcome.schema_version,
            "payload": outcome.payload,
            // The model's opinion of itself, kept where it reads as exactly
            // that.
            //
            // `viryaos_autopilot_decisions.confidence_basis_points` used to
            // receive this number directly. That column holds the brain's own
            // evidence confidence everywhere else, and `next_best_action`
            // parses it into `Confidence` and ranks on it — so a self-report
            // sat in the same column, in the same units, feeding the same
            // ranker as a measurement. The column now receives
            // `evidence_confidence_basis_points`, which does not read this
            // value, and the self-report lives here, named.
            //
            // Not authority either way: `disposition` is a per-kind constant,
            // so a model cannot talk its way to auto-execute by reporting
            // 9500.
            "self_reported_confidence_basis_points":
                outcome.self_reported_confidence.self_reported_basis_points(),
            "confidence_provenance": "model_self_report",
        });

        // The autopilot_decisions.reason column has a CHECK constraint
        // (non-empty, <=240 chars). `payload.rationale` deserializes
        // with `#[serde(default)]` — an outcome that carries no
        // rationale, or only whitespace, used to die here on
        // `reason_check` and the whole outcome was discarded. The
        // absence is itself the honest reason; record it.
        //
        // The LLM rationale can also be longer than 240 chars, so
        // truncate to fit. Use char-based truncation (not byte-based)
        // so multi-byte UTF-8 (Polish diacritics, emoji) doesn't
        // exceed the char_length CHECK. The full rationale is
        // preserved in input_snapshot.payload.rationale.
        let decision_reason = {
            let r = outcome.payload.rationale.trim();
            if r.is_empty() {
                "Outcome supplied no rationale."
            } else if r.chars().count() <= 240 {
                r
            } else {
                let byte_end = r.char_indices().nth(240).map_or(r.len(), |(b, _)| b);
                // Safety: char_indices always lands on a UTF-8 boundary.
                r.get(..byte_end).unwrap_or(r)
            }
        };

        // Insert the decision row. decision_key mirrors the outcome's
        // idempotency_key so a worker retry is a no-op.
        let inserted_decision = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO viryaos_autopilot_decisions (
                id, workspace_id, decision_key, context, subject_kind, subject_id,
                decision_kind, confidence_basis_points, disposition, reason,
                input_snapshot, policy_snapshot, recommendation, trace_id
            )
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
            ON CONFLICT (workspace_id, decision_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(decision_id)
        .bind(outcome.workspace_id)
        .bind(&outcome.idempotency_key)
        .bind(outcome.kind.autopilot_context())
        .bind(subject_kind)
        .bind(subject_id)
        .bind(outcome.kind.decision_kind())
        // NOT the self-report. See `evidence_confidence_basis_points`: an LLM
        // proposal carries no measured evidence confidence, so this column gets
        // the constant saying so rather than a number the model chose for itself.
        .bind(evidence_confidence_basis_points(outcome))
        .bind(outcome.kind.disposition())
        .bind(decision_reason)
        .bind(&input_snapshot)
        .bind(json!({ "source": "agent_outcome", "schema_version": outcome.schema_version }))
        .bind(json!({}))
        .bind(trace_id)
        .fetch_optional(&mut *tx)
        .await?;

        tracing::debug!(
            outcome_id = %outcome.id,
            idempotency_key = %outcome.idempotency_key,
            inserted = inserted_decision.is_some(),
            "decision INSERT result"
        );

        // On a crash-recovery re-run the decision row already exists (conflict).
        // Use the existing id so the action row's FK is valid.
        let decision_id = match inserted_decision {
            Some(id) => id,
            None => {
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT id FROM viryaos_autopilot_decisions \
                     WHERE workspace_id = $1 AND decision_key = $2",
                )
                .bind(outcome.workspace_id)
                .bind(&outcome.idempotency_key)
                .fetch_one(&mut *tx)
                .await?
            }
        };

        // Side tables per kind. Single-writer: only this worker inserts here.
        let mut outreach_target_auto_promoted = false;
        match outcome.kind {
            OutcomeKind::AudienceSegments => {
                if let Some(item) = &outcome.payload.item {
                    self.insert_fan_segment(&mut tx, outcome, item).await?;
                }
            }
            OutcomeKind::OutreachTargets => {
                if let Some(item) = &outcome.payload.item {
                    outreach_target_auto_promoted =
                        self.insert_outreach_target(&mut tx, outcome, item).await?;
                }
            }
            _ => {}
        }

        // Action row only for require_approval kinds — except scout findings.
        // A finding's review act is on the opportunity row itself (the
        // shortlist's own controls progress or dismiss it), and an approved
        // action that no executor claims would sit in `queued` forever,
        // polluting every in-flight index. The decision's require_approval
        // disposition still routes it through the provenance gate.
        let action_id = if outcome.kind.disposition() == "require_approval"
            && outcome.kind != OutcomeKind::OpportunityFindings
        {
            let action_id = Uuid::now_v7();

            // A social_post from the community-engager worker targets a
            // specific community (Reddit, forum) and carries a valid
            // target_id + subreddit. Regular social posts target owned
            // channels (Instagram, Facebook, X) and materialize as campaign
            // drafts. The distinction is in the item payload, not the
            // outcome kind. The target_id must parse as a valid UUID — if
            // it doesn't, the post falls through to the generic content
            // path rather than pointing at a non-existent outreach target.
            let community_target_id = if outcome.kind == OutcomeKind::SocialPost {
                outcome
                    .payload
                    .item
                    .as_ref()
                    .and_then(|i| i.get("platform"))
                    .and_then(Value::as_str)
                    .filter(|p| *p == "reddit")
                    .and_then(|_| {
                        outcome
                            .payload
                            .item
                            .as_ref()
                            .and_then(|i| i.get("target_id"))
                            .and_then(Value::as_str)
                            .and_then(|s| Uuid::parse_str(s).ok())
                    })
            } else {
                None
            };

            // Admission gate: the supplied target_id must name a real,
            // screened-and-admitted community target. Until the topical screen
            // existed, communities were admitted on member count alone — a
            // 360k-member video-game subreddit was admitted and posted to, and
            // the model's target_id needed only to parse as a UUID. Both holes
            // close here: fabricated ids find no row, and refused communities
            // carry no admit.
            // The validated source row survives to the payload build — its
            // media fields are attached there, Rust-side, so the model's
            // text can never carry a URL it invented.
            let mut community_source: Option<CommunityPostSourceRow> = None;
            if let Some(target_id) = community_target_id {
                let admitted = sqlx::query_scalar::<_, bool>(
                    r#"
                    SELECT EXISTS(
                        SELECT 1 FROM agent_outreach_targets
                        WHERE workspace_id = $1
                          AND id = $2
                          AND target_kind = 'community'
                          AND screening_verdict = 'admitted'
                          AND status = 'promoted'
                    )
                    "#,
                )
                .bind(outcome.workspace_id)
                .bind(target_id)
                .fetch_one(&mut *tx)
                .await?;
                if !admitted {
                    let rejection = OutcomeRejection::UnvettedCommunity { target_id };
                    tracing::warn!(
                        outcome_id = %outcome.id,
                        target_id = %target_id,
                        rejection = %rejection,
                        "rejecting community post: target is not an admitted community"
                    );
                    drop(tx);
                    self.reject_outcome(outcome.id, &rejection.to_string())
                        .await?;
                    return Ok((None, None));
                }

                // Source gate: the post must name the trusted content source
                // its facts come from — and which kinds it may name depends
                // on which worker produced the draft. The engager shares
                // release videos; the repost worker carries the band's own
                // synced social posts. Events, releases and stories are real
                // material too, but they belong to other channels.
                //
                // The row is fetched rather than existence-checked because
                // its media fields are what the action payload carries —
                // the model writes the words; the source's own media and
                // permalink are attached here, where the model cannot
                // substitute a URL it invented.
                let allowed_source_kind = match producing_template.as_deref() {
                    Some("community-repost") => "social_post",
                    _ => "video",
                };
                let source_id_raw = outcome
                    .payload
                    .item
                    .as_ref()
                    .and_then(|i| i.get("source_id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let source_row: Option<CommunityPostSourceRow> = match source_id_raw
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s).ok())
                {
                    Some(source_id) => {
                        sqlx::query_as::<_, CommunityPostSourceRow>(
                            r#"
                        SELECT metadata->>'media_url' AS media_url,
                               metadata->>'media_id' AS media_id,
                               metadata->>'media_type' AS media_type,
                               metadata->>'thumbnail_url' AS thumbnail_url,
                               metadata->>'url' AS source_url
                        FROM viryaos_content_sources
                        WHERE workspace_id = $1
                          AND id = $2
                          AND source_kind = $3
                          AND active
                          AND expires_at > now()
                        "#,
                        )
                        .bind(outcome.workspace_id)
                        .bind(source_id)
                        .bind(allowed_source_kind)
                        .fetch_optional(&mut *tx)
                        .await?
                    }
                    None => None,
                };
                let Some(source_row) = source_row else {
                    let rejection = OutcomeRejection::UnsourcedPost {
                        source_id: source_id_raw,
                    };
                    tracing::warn!(
                        outcome_id = %outcome.id,
                        rejection = %rejection,
                        "rejecting community post: no live content source behind it"
                    );
                    drop(tx);
                    self.reject_outcome(outcome.id, &rejection.to_string())
                        .await?;
                    return Ok((None, None));
                };
                community_source = Some(source_row);
            }

            // Check if the workspace's policy for the outcome's context is
            // set to bounded_auto. If so, the action skips the approval step
            // and goes straight to queued. Two cases:
            //   1. Reddit community posts (promotion_budget context) — the
            //      community executor's anti-spam guardrails (3 posts/24h,
            //      7-day subreddit cooldown) serve as the bounds.
            //   2. Signal pushes (fan_lifecycle context) — pushing to an
            //      existing fan who opted in is fan lifecycle engagement,
            //      not promotion spend. The push delivery rate limits and
            //      the fan's own opt-in serve as the bounds.
            // Press pitches and regular social posts always require human
            // approval because they reach external audiences directly.
            let is_reddit_community_post = community_target_id.is_some();
            let is_signal_push = outcome.kind == OutcomeKind::SignalPush;
            // A channel whose auto-post flag is set already carries the
            // operator's approval; asking again per post is asking twice, and
            // the second ask is what expired. See `auto_post_platforms` for
            // what that cost. Reddit cannot reach this: `permits` refuses it,
            // and the executor needs `CROWDRELAY_REDDIT_WRITE_ENABLED` on top
            // of the community auto-post flag besides.
            let draft_platform = outcome
                .payload
                .item
                .as_ref()
                .and_then(|i| i.get("platform"))
                .and_then(Value::as_str);
            let channel_pre_approved = outcome.kind == OutcomeKind::SocialPost
                && community_target_id.is_none()
                && self.auto_post_platforms.permits(draft_platform);

            let auto_execute = if is_reddit_community_post {
                self.is_context_bounded_auto(&mut tx, "promotion_budget")
                    .await?
            } else if is_signal_push {
                self.is_context_bounded_auto(&mut tx, "fan_lifecycle")
                    .await?
            } else {
                channel_pre_approved
            };
            if channel_pre_approved {
                tracing::info!(
                    outcome_id = %outcome.id,
                    platform = draft_platform.unwrap_or("unknown"),
                    "channel has standing operator approval; dispatching without a second one"
                );
            }

            let action_details = if let Some(target_id) = community_target_id {
                Some(
                    self.community_engagement_action(
                        &mut tx,
                        outcome,
                        target_id,
                        community_source.as_ref(),
                    )
                    .await?,
                )
            } else {
                match outcome.kind {
                    OutcomeKind::PressPitch => {
                        // A pitch is an email to a named journalist, so it
                        // needs an address. Without one it becomes a succeeded
                        // action that reaches nobody: no executor claims
                        // `press-pitch`, and the outbox event carried a draft
                        // with no recipient. Every press pitch production ever
                        // produced ended that way.
                        //
                        // Refusing beats drafting into the void. The pitch
                        // costs a model call and a dispatch slot, and an
                        // operator approving copy addressed to nobody is worse
                        // than never being asked.
                        let Some(recipient) =
                            press_recipient(&self.pool, self.workspace_id).await?
                        else {
                            return Err(AgentOutcomeError::NoPressRecipient);
                        };
                        Some((
                            json!({
                                "kind": "request_agent_content",
                                "template_id": "press-pitch",
                                "task_id": outcome.task_id,
                                "draft": outcome.payload.item.clone().unwrap_or(Value::Null),
                                "recipient_email": recipient.contact_email,
                                "recipient_name": recipient.display_name,
                                "recipient_target_id": recipient.id,
                            }),
                            "agent.content.request",
                        ))
                    }
                    OutcomeKind::SocialPost => Some((
                        json!({
                            "kind": "request_agent_content",
                            "task_id": outcome.task_id,
                            "draft": outcome.payload.item.clone().unwrap_or(Value::Null),
                        }),
                        "agent.content.request",
                    )),
                    OutcomeKind::SignalPush => {
                        let item = outcome.payload.item.as_ref();
                        let title = item
                            .and_then(|i| i.get("title"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let body = item
                            .and_then(|i| i.get("body"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let target_path = item
                            .and_then(|i| i.get("target_path"))
                            .and_then(Value::as_str)
                            .map(|s| s.to_owned());
                        let event_id = item
                            .and_then(|i| i.get("event_id"))
                            .and_then(Value::as_str)
                            .and_then(|s| Uuid::parse_str(s).ok());
                        let segment = item
                            .and_then(|i| i.get("segment"))
                            .and_then(Value::as_str)
                            .map(|s| s.to_owned());
                        // The approval quotes the audience the send would
                        // reach today — the same eligibility and envelope
                        // bound the execution path enforces. A segment the
                        // count cannot resolve stays uncounted rather than
                        // failing the outcome: the send path still refuses
                        // the bad slug at dispatch.
                        let audience = crowdrelay_infra::autopilot::signal_push_audience(
                            &self.pool,
                            self.workspace_id,
                            segment.as_deref(),
                        )
                        .await;
                        let (audience_size, audience_basis) = match audience {
                            Ok(audience) => {
                                let basis = match &segment {
                                    Some(slug) if audience.reached < audience.eligible => {
                                        format!(
                                            "fans in the '{slug}' segment with notifications on who consented to marketing — the workspace's per-step send envelope caps this push at {}",
                                            audience.reached
                                        )
                                    }
                                    Some(slug) => format!(
                                        "fans in the '{slug}' segment with notifications on who consented to marketing"
                                    ),
                                    None if audience.reached < audience.eligible => format!(
                                        "fans with notifications on who consented to marketing — the workspace's per-step send envelope caps this push at {}",
                                        audience.reached
                                    ),
                                    None => "fans with notifications on who consented to marketing"
                                        .to_owned(),
                                };
                                (Some(audience.reached), basis)
                            }
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    segment = segment.as_deref(),
                                    "signal push audience could not be counted at raise"
                                );
                                (None, String::new())
                            }
                        };
                        Some((
                            json!({
                                "kind": "request_signal_push",
                                "task_id": outcome.task_id,
                                "title": title,
                                "body": body,
                                "target_path": target_path,
                                "event_id": event_id,
                                "segment": segment,
                                "audience_size": audience_size,
                                "audience_basis": audience_basis,
                            }),
                            "signal.push.request",
                        ))
                    }
                    OutcomeKind::OutreachTargets => {
                        // Community targets (Reddit, forums) are auto-promoted
                        // in insert_outreach_target — no approval action needed.
                        if outreach_target_auto_promoted {
                            None
                        } else {
                            let item = outcome.payload.item.as_ref();
                            let target_kind = item
                                .and_then(|i| i.get("target_kind"))
                                .and_then(Value::as_str)
                                .unwrap_or("unknown");
                            let display_name = item
                                .and_then(|i| i.get("display_name"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let contact_email = item
                                .and_then(|i| i.get("contact_email"))
                                .and_then(Value::as_str)
                                .map(|s| s.to_owned());
                            let contact_domain = item
                                .and_then(|i| i.get("contact_domain"))
                                .and_then(Value::as_str)
                                .map(|s| s.to_owned());
                            let why_fit = item
                                .and_then(|i| i.get("why_fit"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let evidence_urls = item
                                .and_then(|i| i.get("evidence_urls"))
                                .cloned()
                                .unwrap_or(json!([]));
                            let subreddit = item
                                .and_then(|i| i.get("subreddit"))
                                .and_then(Value::as_str)
                                .map(|s| s.to_owned());
                            Some((
                                json!({
                                    "kind": "request_outreach_target",
                                    "task_id": outcome.task_id,
                                    "target_kind": target_kind,
                                    "display_name": display_name,
                                    "contact_email": contact_email,
                                    "contact_domain": contact_domain,
                                    "why_fit": why_fit,
                                    "evidence_urls": evidence_urls,
                                    "subreddit": subreddit,
                                }),
                                "outreach.target.request",
                            ))
                        }
                    }
                    // An unhandled require_approval kind should not produce a
                    // garbage action with a null payload. Log and skip — the
                    // outcome row is still preserved for audit.
                    _ => {
                        tracing::warn!(
                            kind = %outcome.kind.as_str(),
                            task_id = %outcome.task_id,
                            "require_approval outcome has no action mapping — skipping action creation"
                        );
                        None
                    }
                }
            };
            let inserted_action = if let Some((payload, action_kind)) = action_details {
                // The durable class derives from the payload itself, never
                // from a literal beside it: a press pitch carrying a recipient
                // is third-party, and a hand-written `first_party_reversible`
                // here once let one walk past the evidence gate.
                let parsed = serde_json::from_value::<
                    crowdrelay_application::autopilot::AutopilotActionPayload,
                >(payload.clone())
                .map_err(|error| {
                    tracing::warn!(
                        %error,
                        "agent outcome payload does not match its schema — refusing to persist an action that cannot execute"
                    );
                    AgentOutcomeError::UnpersistablePayload
                })?;
                let action_class = parsed.action_class().as_str();

                if auto_execute {
                    sqlx::query_scalar::<_, Uuid>(
                        r#"
                    INSERT INTO viryaos_autopilot_actions (
                        id, workspace_id, decision_id, context, action_kind,
                        subject_kind, subject_id, idempotency_key, payload, status,
                        action_class, approved_at, approved_by, approval_expires_at,
                        trace_id, causation_id
                    )
                    VALUES (
                        $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
                        now(), 'policy:bounded_auto', NULL,
                        $12, NULL
                    )
                    ON CONFLICT DO NOTHING
                    RETURNING id
                    "#,
                    )
                    .bind(action_id)
                    .bind(outcome.workspace_id)
                    .bind(decision_id)
                    .bind(outcome.kind.autopilot_context())
                    .bind(action_kind)
                    .bind("agent_outcome")
                    .bind(outcome.id)
                    .bind(&outcome.idempotency_key)
                    .bind(&payload)
                    .bind("queued")
                    .bind(action_class)
                    .bind(trace_id)
                    .fetch_optional(&mut *tx)
                    .await?
                } else {
                    let inserted = sqlx::query_scalar::<_, Uuid>(
                        r#"
                    INSERT INTO viryaos_autopilot_actions (
                        id, workspace_id, decision_id, context, action_kind,
                        subject_kind, subject_id, idempotency_key, payload, status,
                        action_class, approved_at, approved_by, approval_expires_at,
                        trace_id, causation_id
                    )
                    VALUES (
                        $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
                        NULL, NULL,
                        now() + INTERVAL '72 hours',
                        $12, NULL
                    )
                    ON CONFLICT DO NOTHING
                    RETURNING id
                    "#,
                    )
                    .bind(action_id)
                    .bind(outcome.workspace_id)
                    .bind(decision_id)
                    .bind(outcome.kind.autopilot_context())
                    .bind(action_kind)
                    .bind("agent_outcome")
                    .bind(outcome.id)
                    .bind(&outcome.idempotency_key)
                    .bind(&payload)
                    .bind("awaiting_approval")
                    .bind(action_class)
                    .bind(trace_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                    // A parked approval nobody hears about is a decision that
                    // never happened. `decisions/persist.rs` emits
                    // `approval_requested` for the brain's own candidates;
                    // agent-outcome actions insert on a different path and
                    // parked silently — outreach targets sat for hours with
                    // no Discord alert because no event ever left. Same
                    // transaction: action + notification commit or neither
                    // does. Only on a real insert — a conflict is a re-run
                    // and must not re-notify.
                    if let Some(inserted_id) = inserted {
                        sqlx::query(
                            r#"
                            INSERT INTO outbox_events (workspace_id, event_type, event_version, payload, max_attempts, trace_id, causation_id, action_id)
                            VALUES (
                                $1, 'crowdrelay.autopilot.approval_requested', 1,
                                jsonb_build_object(
                                    'action_id', $2::uuid,
                                    'context', $3::text,
                                    'action_kind', $4::text,
                                    'subject_kind', $5::text,
                                    'subject_id', $6::uuid,
                                    'reason', $7::text,
                                    'confidence_basis_points', $8::integer,
                                    'approval_expires_at', now() + INTERVAL '72 hours',
                                    'trace_id', $9::uuid
                                ),
                                12,
                                $9,
                                $10,
                                $2
                            )
                            "#,
                        )
                        .bind(outcome.workspace_id)
                        .bind(inserted_id)
                        .bind(outcome.kind.autopilot_context())
                        .bind(action_kind)
                        .bind("agent_outcome")
                        .bind(outcome.id)
                        .bind(decision_reason)
                        .bind(evidence_confidence_basis_points(outcome))
                        .bind(trace_id)
                        .bind(decision_id)
                        .execute(&mut *tx)
                        .await?;
                    }
                    inserted
                }
            } else {
                None
            };
            tracing::debug!(
                outcome_id = %outcome.id,
                idempotency_key = %outcome.idempotency_key,
                action_inserted = inserted_action.is_some(),
                "action INSERT result"
            );
            // On a re-run the action row already exists (conflict). Resolve
            // the existing id so processed_action_id is not NULL. When the
            // outcome had no action mapping (None payload), no row exists —
            // fetch_optional returns None instead of erroring.
            match inserted_action {
                Some(id) => Some(id),
                None => sqlx::query_scalar::<_, Option<Uuid>>(
                    "SELECT id FROM viryaos_autopilot_actions \
                 WHERE workspace_id = $1 AND idempotency_key = $2",
                )
                .bind(outcome.workspace_id)
                .bind(&outcome.idempotency_key)
                .fetch_optional(&mut *tx)
                .await?
                .flatten(),
            }
        } else {
            None
        };

        // Mark the outcome processed in the same transaction so a crash
        // between the decision/action inserts and the status update can
        // never leave the row stuck in 'processing' (which the poll query
        // never re-selects).
        //
        // The resolved trace is written back here too. `/v1/admin/ops/trace/{id}`
        // reads `agent_outcomes` as one branch of the timeline, and that branch
        // was dead for every row: the agents service never populates the column,
        // so the outcome that caused a decision did not appear in the timeline
        // it caused. Writing it on the row we are already updating costs
        // nothing, needs no change to the agents service, and is what makes the
        // agent step visible in the causal chain rather than inferred from the
        // decision's `input_snapshot`.
        //
        // Left nullable in the schema on purpose: this column belongs to a table
        // an external service writes, and narrowing a column whose other writer
        // is outside this repository is how a migration breaks a service nobody
        // deployed.
        sqlx::query(
            r#"
            UPDATE agent_outcomes
            SET status = 'processed',
                processed_decision_id = $2,
                processed_action_id = $3,
                processed_at = now(),
                trace_id = COALESCE(trace_id, $4)
            WHERE id = $1
            "#,
        )
        .bind(outcome.id)
        .bind(decision_id)
        .bind(action_id)
        .bind(trace_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok((Some(decision_id), action_id))
    }

    /// Creates a tracked smart link for an agent-produced content item so
    /// clicks from the posted content can be attributed back to the agent
    /// channel. Called within the `map_outcome` transaction.
    ///
    /// The slug is deterministic: `agent-{outcome.id.simple()}`. This
    /// makes re-runs idempotent (ON CONFLICT DO UPDATE) and keeps agent-
    /// created links identifiable in the admin smart-links list.
    ///
    /// Returns the public redirect path (`/l/{slug}`) or `None` if the item
    /// has no usable destination URL.
    async fn ensure_agent_smart_link(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        outcome: &ValidatedOutcome,
        destination: &str,
        channel_source: &str,
        channel_community: Option<&str>,
    ) -> Result<Option<String>, AgentOutcomeError> {
        // The destination came out of a language model. `starts_with("http")`
        // was the whole of the check, so the tenant's own domain answered a
        // 302 to wherever the model said — an open redirect under the band's
        // name, seeded by a model, with the fan's trust in the domain doing
        // the work.
        //
        // It broke attribution too, and quietly: the whole point of the link
        // is that the far end is a page that captures the visitor. A foreign
        // destination counts the click, never creates the fan, and the post is
        // measured as reach that converted nobody.
        //
        // Refusing returns `None`, which the caller already handles — the post
        // goes out untracked rather than not at all. A post with no link is
        // worth less than a post with one; a post that redirects the audience
        // somewhere nobody approved is worth less than either.
        let destination = match crowdrelay_domain::acquisition::agent_smart_link_destination(
            destination,
            &[self.public_origin.as_str()],
        ) {
            Ok(destination) => destination,
            Err(refusal) => {
                tracing::warn!(
                    outcome_id = %outcome.id,
                    workspace_id = %workspace_id,
                    refusal = %refusal,
                    "refused an agent smart-link destination; the post will go out untracked"
                );
                return Ok(None);
            }
        };
        let destination = destination.as_str();
        // The smart_links table enforces UNIQUE(workspace_id, slug) so
        // re-runs of the same outcome won't create duplicates — the ON
        // CONFLICT DO UPDATE handles that. The full simple UUID (32 hex
        // chars) keeps the slug well under the 128-char CHECK constraint.
        let slug = format!("agent-{}", outcome.id.simple());

        sqlx::query(
            r#"
            INSERT INTO smart_links
                (workspace_id, slug, destination_url, active,
                 channel_source, channel_community)
            VALUES ($1, $2, $3, true, $4, $5)
            ON CONFLICT (workspace_id, slug) DO UPDATE SET
                destination_url = EXCLUDED.destination_url,
                active = true
            "#,
        )
        .bind(workspace_id)
        .bind(&slug)
        .bind(destination)
        .bind(channel_source)
        .bind(channel_community)
        .execute(&mut **tx)
        .await
        .map_err(AgentOutcomeError::from)?;

        Ok(Some(format!("/l/{slug}")))
    }

    /// Inserts an `agent_fan_segments` row from an audience_segments item.
    /// `UNIQUE (workspace_id, name)` makes a re-run a no-op.
    async fn insert_fan_segment(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        outcome: &ValidatedOutcome,
        item: &Value,
    ) -> Result<(), AgentOutcomeError> {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Unnamed segment");
        let description = item
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let size_estimate = item
            .get("size_estimate")
            .and_then(Value::as_i64)
            .map(i32::try_from)
            .and_then(Result::ok);
        let criteria = item.get("criteria").cloned().unwrap_or(json!({}));
        sqlx::query(
            r#"
            INSERT INTO agent_fan_segments
                (workspace_id, name, description, size_estimate, criteria, source_task_id)
            VALUES ($1,$2,$3,$4,$5,$6)
            ON CONFLICT (workspace_id, name) DO NOTHING
            "#,
        )
        .bind(outcome.workspace_id)
        .bind(name)
        .bind(description)
        .bind(size_estimate)
        .bind(&criteria)
        .bind(outcome.task_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Inserts an `agent_outreach_targets` staging row. For personal-contact
    /// kinds (press, radio, etc.) the row is `proposed` — operator
    /// verification is the approval that flips it. For `community`-kind
    /// targets (Reddit subreddits, forums) the row is auto-promoted to
    /// `promoted` because these are public spaces, not personal contacts —
    /// the growth loop can engage them without operator review.
    ///
    /// `target_kind` arrives from the agent's own JSON and goes into a
    /// CHECK-constrained column, so it is checked here rather than left to
    /// Postgres. `validate` gates the outcome's schema version, kind,
    /// confidence and payload shape, but not this field, and an unaccepted
    /// value therefore reached the INSERT and raised `check_violation` —
    /// rolling back the decision and action rows written earlier in the same
    /// transaction and rejecting the outcome with a raw database error instead
    /// of a statement about the field.
    ///
    /// Rejecting is right; being unable to say why was not. The previous
    /// `unwrap_or("press")` was the other half of the same gap: an agent that
    /// omitted the field got its target filed as press, which is not a default
    /// so much as a guess about which outreach playbook to run.
    async fn insert_outreach_target(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        outcome: &ValidatedOutcome,
        item: &Value,
    ) -> Result<bool, AgentOutcomeError> {
        let target_kind = item
            .get("target_kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !AGENT_TARGET_KINDS.contains(&target_kind) {
            return Err(AgentOutcomeError::UnknownTargetKind(target_kind.to_owned()));
        }
        let display_name = item
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let contact_email = item.get("contact_email").and_then(Value::as_str);
        let contact_domain = item.get("contact_domain").and_then(Value::as_str);
        let why_fit = item.get("why_fit").and_then(Value::as_str).unwrap_or("");
        let evidence = item.get("evidence_urls").cloned().unwrap_or(json!([]));
        let subreddit = item.get("subreddit").and_then(Value::as_str);
        // The discovering agent scraped the subreddit — it knows the
        // language its posts are written in, and the repost drafter writes
        // in it. Bounded to the column's shape; garbage truncates to NULL
        // rather than failing the insert.
        let language = item
            .get("language")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.chars().count() <= 8)
            .map(str::to_owned);
        // Community targets (Reddit subreddits, forums) are public spaces.
        // Auto-promote them so the brain's community-engager can dispatch
        // without waiting for operator review. Personal-contact kinds keep
        // the proposed → promoted operator-approval flow.
        //
        // Auto-promotion is not the same as unscreened. A community an agent
        // named still has to clear the screening policy — evidence, size,
        // plausible activity, its own self-promo rules, and our recorded
        // judgement about it — before the growth loop will post there.
        // The verdict is recorded so a refusal survives the next scan
        // instead of being rediscovered and re-proposed every week.
        let is_community = target_kind == "community";
        let initial_status = if is_community { "promoted" } else { "proposed" };
        let (place_id, verdict, refusal) = if is_community {
            let place = community_place(tx, outcome.workspace_id, subreddit).await?;
            let snapshot = community_snapshot(&evidence, place.as_ref());
            match screen_community_candidate(&snapshot, TargetDiscoveryPolicy::default()) {
                ScreeningVerdict::Admit { .. } => (place.map(|p| p.id), Some("admitted"), None),
                ScreeningVerdict::Refuse(reason) => {
                    (place.map(|p| p.id), Some("refused"), Some(reason.as_str()))
                }
            }
        } else {
            (None, None, None)
        };
        // A community's identity is its subreddit, not its display name —
        // the scanner may name the same sub "r/deathcore" one week and
        // "Deathcore — news & discussion" the next, and display-name dedup
        // let both live as separate promoted targets (eleven subreddits sat
        // doubled in production, each drafted and posted to twice per wave).
        // The subreddit-arbiter upsert keeps one row per community: a
        // re-proposal lands on the existing row and is re-screened there,
        // with status sticky so a discarded community does not resurrect.
        // Personal-contact kinds keep display-name dedup — a person and a
        // place do not share an identity.
        let community_identity = is_community && subreddit.is_some_and(|s| !s.trim().is_empty());
        let sql = if community_identity {
            r#"
            INSERT INTO agent_outreach_targets
                (workspace_id, target_kind, display_name, contact_email, contact_domain,
                 why_fit, evidence, source_task_id, subreddit, status,
                 place_id, screening_verdict, refusal_reason, screened_at, language)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,
                    CASE WHEN $2::text = 'community' THEN normalize_subreddit($9) ELSE $9 END,
                    $10,$11,$12,$13,
                    CASE WHEN $12::text IS NULL THEN NULL ELSE now() END, $14)
            ON CONFLICT (workspace_id, normalize_subreddit(subreddit))
                WHERE target_kind = 'community'
                  AND subreddit IS NOT NULL
                  AND normalize_subreddit(subreddit) <> ''
            DO UPDATE SET
                subreddit = COALESCE(EXCLUDED.subreddit, agent_outreach_targets.subreddit),
                language = COALESCE(EXCLUDED.language, agent_outreach_targets.language),
                status = CASE
                    WHEN agent_outreach_targets.status = 'discarded' THEN agent_outreach_targets.status
                    WHEN EXCLUDED.status = 'promoted' THEN 'promoted'
                    ELSE agent_outreach_targets.status
                END,
                place_id = COALESCE(EXCLUDED.place_id, agent_outreach_targets.place_id),
                -- A re-proposal is re-screened against whatever the audience
                -- graph knows now, which is how a community that was refused
                -- for being too small gets readmitted once it has grown. The
                -- verdict is only overwritten when this pass produced one.
                screening_verdict = COALESCE(EXCLUDED.screening_verdict, agent_outreach_targets.screening_verdict),
                refusal_reason = CASE
                    WHEN EXCLUDED.screening_verdict IS NULL THEN agent_outreach_targets.refusal_reason
                    ELSE EXCLUDED.refusal_reason
                END,
                screened_at = COALESCE(EXCLUDED.screened_at, agent_outreach_targets.screened_at),
                updated_at = now()
            "#
        } else {
            r#"
            INSERT INTO agent_outreach_targets
                (workspace_id, target_kind, display_name, contact_email, contact_domain,
                 why_fit, evidence, source_task_id, subreddit, status,
                 place_id, screening_verdict, refusal_reason, screened_at, language)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,
                    CASE WHEN $12::text IS NULL THEN NULL ELSE now() END, $14)
            ON CONFLICT (workspace_id, display_name, target_kind) DO UPDATE SET
                subreddit = COALESCE(EXCLUDED.subreddit, agent_outreach_targets.subreddit),
                language = COALESCE(EXCLUDED.language, agent_outreach_targets.language),
                status = CASE
                    WHEN agent_outreach_targets.status = 'discarded' THEN agent_outreach_targets.status
                    WHEN EXCLUDED.status = 'promoted' THEN 'promoted'
                    ELSE agent_outreach_targets.status
                END,
                place_id = COALESCE(EXCLUDED.place_id, agent_outreach_targets.place_id),
                -- A re-proposal is re-screened against whatever the audience
                -- graph knows now, which is how a community that was refused
                -- for being too small gets readmitted once it has grown. The
                -- verdict is only overwritten when this pass produced one.
                screening_verdict = COALESCE(EXCLUDED.screening_verdict, agent_outreach_targets.screening_verdict),
                refusal_reason = CASE
                    WHEN EXCLUDED.screening_verdict IS NULL THEN agent_outreach_targets.refusal_reason
                    ELSE EXCLUDED.refusal_reason
                END,
                screened_at = COALESCE(EXCLUDED.screened_at, agent_outreach_targets.screened_at),
                updated_at = now()
            "#
        };
        sqlx::query(sql)
            .bind(outcome.workspace_id)
            .bind(target_kind)
            .bind(display_name)
            .bind(contact_email)
            .bind(contact_domain)
            .bind(why_fit)
            .bind(&evidence)
            .bind(outcome.task_id)
            .bind(subreddit)
            .bind(initial_status)
            .bind(place_id)
            .bind(verdict)
            .bind(refusal)
            .bind(language)
            .execute(&mut **tx)
            .await?;
        // Community targets are auto-promoted — the operator does not need
        // to approve them. Personal-contact kinds keep the proposed → promoted
        // operator-approval flow and need an action row.
        Ok(is_community)
    }

    async fn reject_outcome(
        &self,
        outcome_id: Uuid,
        reason: &str,
    ) -> Result<(), AgentOutcomeError> {
        sqlx::query(
            r#"
            UPDATE agent_outcomes
            SET status = 'rejected', rejection_reason = $3
            WHERE id = $1 AND workspace_id = $2
            "#,
        )
        .bind(outcome_id)
        .bind(self.workspace_id.into_uuid())
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Checks whether the workspace's autopilot policy for the given context
    /// is set to `bounded_auto`. This is the gate for autonomous execution:
    /// if the operator has set the policy to `bounded_auto`, the action
    /// skips the approval step and goes straight to `queued`. Returns
    /// `false` if the policy is missing or not `bounded_auto` — fail-closed
    /// to `require_approval`.
    async fn is_context_bounded_auto(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        context: &str,
    ) -> Result<bool, AgentOutcomeError> {
        let autonomy: Option<String> = sqlx::query_scalar(
            r#"
            SELECT autonomy_level
            FROM viryaos_autopilot_policies
            WHERE workspace_id = $1 AND context = $2
            LIMIT 1
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(context)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(autonomy.as_deref() == Some("bounded_auto"))
    }
}

#[derive(sqlx::FromRow)]
struct OutcomeRow {
    id: Uuid,
    workspace_id: Uuid,
    task_id: Uuid,
    result_id: Uuid,
    kind: String,
    schema_version: i32,
    payload: Value,
    confidence_basis_points: i32,
    idempotency_key: String,
    trace_id: Option<Uuid>,
}

/// The validated content source behind a community post — read by the source
/// gate, carried into the action payload so the media the post ships is the
/// source's own, never a URL the model produced.
#[derive(sqlx::FromRow)]
struct CommunityPostSourceRow {
    media_url: Option<String>,
    media_id: Option<String>,
    media_type: Option<String>,
    thumbnail_url: Option<String>,
    source_url: Option<String>,
}

#[cfg(test)]
mod tests;
