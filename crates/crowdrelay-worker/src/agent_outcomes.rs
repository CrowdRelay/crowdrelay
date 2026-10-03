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
use crate::community_vetting::{
    community_place, community_place_by_url, community_snapshot, ensure_community_place,
};
use std::time::Duration;

use crowdrelay_application::agent_outcomes::{
    OutcomeKind, ProvenanceRejection, ValidatedOutcome, evidence_confidence_basis_points,
    provenance_admission, validate,
};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::action_class::{ActionClass, effective_authority};
use crowdrelay_domain::audience_graph::canonical_place_url;
use crowdrelay_domain::autonomy::AutonomyLevel;
use crowdrelay_domain::standing_approval::{
    StandingGrant, UnattendedAuthority, unattended_authority,
};
use crowdrelay_domain::target_discovery::{
    ScreeningVerdict, TargetDiscoveryPolicy, screen_community_candidate,
};
use crowdrelay_domain::worker_template::{TemplateAudience, WorkerTemplate};
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
include!("agent_outcomes/authority.rs");

const BATCH_LIMIT: i64 = 32;

/// The `agent_outreach_targets_target_kind_check` vocabulary (migration 0138).
///
/// Deliberately not `OutreachTargetKind`. That enum is the vocabulary of
/// `outreach_targets`, a different table in a different bounded
/// context, and the two sets genuinely differ: this one accepts `community`
/// and rejects `support_slot`, which is the reverse of the other. Reaching for
/// the enum because the column names match would accept `support_slot` here
/// and hand it straight to the CHECK that forbids it.
const AGENT_TARGET_KINDS: [&str; 8] = [
    "press",
    "radio",
    "playlist",
    "media_patronage",
    "endorsement",
    "creator",
    "community",
    "organiser",
];

/// How long an outcome keeps being retried through transient database
/// failures before it is refused.
const TRANSIENT_RETRY_WINDOW: time::Duration = time::Duration::hours(24);

#[derive(Debug, Error)]
pub enum AgentOutcomeError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("validation error: {0}")]
    Validation(#[from] crowdrelay_application::agent_outcomes::OutcomeValidationError),
    #[error("agent proposed target_kind {0:?}, which agent_outreach_targets does not accept")]
    UnknownTargetKind(String),
    #[error(
        "the pitch does not name exactly one outreach target that can be mailed now \
         (active, accepts outreach, not do-not-contact, has an address, outside the \
         contact cooldown) — it is refused rather than sent to someone it was not written for"
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

/// A prospect write fails only in the database, and that failure is the same
/// one the retry classification above already understands — so it is the same
/// variant, not a new one that would be refused instead of retried.
impl From<crowdrelay_infra::fan_prospects::ProspectError> for AgentOutcomeError {
    fn from(error: crowdrelay_infra::fan_prospects::ProspectError) -> Self {
        match error {
            crowdrelay_infra::fan_prospects::ProspectError::Database(error) => {
                Self::Database(error)
            }
        }
    }
}

include!("agent_outcomes/rejections.rs");

include!("agent_outcomes/community_ingestion.rs");
include!("agent_outcomes/beacon_candidates.rs");
include!("agent_outcomes/contact_research.rs");
include!("agent_outcomes/fan_prospects.rs");
include!("agent_outcomes/quality_guard.rs");
include!("agent_outcomes/opportunity_findings.rs");
include!("agent_outcomes/strategy_proposals.rs");
include!("agent_outcomes/creative_family.rs");
include!("agent_outcomes/community_engagement.rs");
include!("agent_outcomes/room_reading.rs");
include!("agent_outcomes/social_platform.rs");

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
                      payload, confidence_basis_points, idempotency_key, trace_id,
                      created_at
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(BATCH_LIMIT)
        .fetch_all(&self.pool)
        .await?;

        let mut processed = 0;
        for row in rows {
            let row_created_at = row.created_at;
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
                // A database that was briefly unavailable says nothing about
                // the outcome. Rejecting here is terminal: on 2026-09-22 four
                // generated press pitches were thrown away over one pool
                // timeout. The row stays `processing`, and the stale-claim
                // recovery returns it to `pending` on a later run. A day of
                // that is no longer a blip, and the outcome is refused then.
                Err(AgentOutcomeError::Database(error))
                    if crowdrelay_infra::database::is_transient_sqlx_error(&error)
                        && time::OffsetDateTime::now_utc() - row_created_at
                            < TRANSIENT_RETRY_WINDOW =>
                {
                    tracing::warn!(
                        outcome_id = %outcome.id,
                        error = %error,
                        "agent outcome hit a transient database failure; left for retry"
                    );
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
            LEFT JOIN autopilot_actions AS action
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

        // A measured channel choice is product policy, not model discretion.
        // Enforce it before opening the transaction so a rejected draft creates
        // no decision/action side effects.
        if outcome.kind == OutcomeKind::SocialPost
            && let Some(expected) = self.selected_social_platform(outcome).await
        {
            let found = outcome
                .payload
                .item
                .as_ref()
                .and_then(|item| item.get("platform"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if found.as_deref() != Some(expected.as_str()) {
                return Err(OutcomeRejection::SocialPlatformMismatch { expected, found }.into());
            }
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
        let producing_task = self.community_producing_task(outcome).await;
        let producing_template = producing_task
            .as_ref()
            .map(|(template, _, _)| template.as_str());
        // FAN SCOUT person evidence uses the canonical prospect repository,
        // whose write is idempotent and owns its own transaction. Resolve it
        // before this decision transaction opens: a transient failure after
        // the observation merely retries the audit decision; it cannot
        // duplicate the person or the observation.
        let fan_prospect_subject = if outcome.kind == OutcomeKind::FanProspects {
            Some(
                self.fan_prospect_subject(outcome, producing_task.as_ref())
                    .await?,
            )
        } else {
            None
        };
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
            OutcomeKind::BeaconCandidates => {
                beacon_candidate_subject(&mut tx, outcome, producing_task.as_ref()).await?
            }
            OutcomeKind::ContactResearch => {
                contact_research_subject(&mut tx, outcome, producing_task.as_ref()).await?
            }
            OutcomeKind::FanProspects => {
                fan_prospect_subject.unwrap_or(("agent_outcome", outcome.id))
            }
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
            // `autopilot_decisions.confidence_basis_points` used to
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
        let decision_reason_owned = strategic_review_hold_reason(outcome);
        let decision_reason_source = decision_reason_owned.as_deref().unwrap_or_else(|| {
            let rationale = outcome.payload.rationale.trim();
            if rationale.is_empty() {
                "Outcome supplied no rationale."
            } else {
                rationale
            }
        });
        let decision_reason = if decision_reason_source.chars().count() <= 240 {
            decision_reason_source
        } else {
            let byte_end = decision_reason_source
                .char_indices()
                .nth(240)
                .map_or(decision_reason_source.len(), |(byte, _)| byte);
            decision_reason_source
                .get(..byte_end)
                .unwrap_or(decision_reason_source)
        };

        // A social_post from the community-engager worker targets a specific
        // community (Reddit, forum) and carries a valid target_id + subreddit.
        // Regular social posts target owned channels (Instagram, Facebook, X)
        // and materialize as campaign drafts. The distinction is in the item
        // payload, not the outcome kind. The target_id must parse as a valid
        // UUID — if it doesn't, the post falls through to the generic content
        // path rather than pointing at a non-existent outreach target.
        //
        // Read here rather than beside the action insert because the context
        // it decides is written on the decision row too, and a decision filed
        // under one context with its action under another is a timeline that
        // does not join.
        let community_target_id = community_target_id(outcome);
        if let Some(target_id) = community_target_id
            && let Some(pinned) = producing_task
                .as_ref()
                .and_then(|(_, prompt, _)| pinned_community_uuid(prompt, "target_id"))
            && pinned != target_id
        {
            return Err(OutcomeRejection::UnvettedCommunity { target_id }.into());
        }
        let effective_context = effective_context(outcome, community_target_id);

        // Insert the decision row. decision_key mirrors the outcome's
        // idempotency_key so a worker retry is a no-op.
        let inserted_decision = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO autopilot_decisions (
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
        .bind(effective_context)
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
                    "SELECT id FROM autopilot_decisions \
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
                    outreach_target_auto_promoted = self
                        .insert_outreach_target(&mut tx, outcome, item, producing_task.as_ref())
                        .await?
                        .0;
                }
            }
            OutcomeKind::StrategyProposals => {
                if let Some(item) = &outcome.payload.item {
                    self.evaluate_strategy_proposals(
                        &mut tx,
                        outcome,
                        item,
                        producing_task.as_ref(),
                    )
                    .await?;
                }
            }
            _ => {}
        }

        // Action row only for require_approval kinds — except scout findings
        // and strategy proposals. A finding's review act is on the
        // opportunity row itself (the shortlist's own controls progress or
        // dismiss it), and a proposal's record is its verdict row — an
        // approved action that no executor claims would sit in `queued`
        // forever, polluting every in-flight index. The decision's
        // require_approval disposition still routes both through the
        // provenance gate.
        // An item-less outreach_targets outcome is the emit side's honest
        // empty — "looked, found nothing". It records a decision (the audit
        // trail that the scan ran) but there is no target to approve, so an
        // awaiting_approval row would be a phantom. The same holds for every
        // remaining require_approval kind: a social_post, press_pitch or
        // signal_push with no item is a refusal, and its action row would
        // carry `draft: null` — an approval card that publishes nothing,
        // which for a poster template would even seed an empty post row.
        let action_id = if outcome.kind.disposition() == "require_approval"
            && outcome.kind != OutcomeKind::OpportunityFindings
            && outcome.kind != OutcomeKind::BeaconCandidates
            && outcome.kind != OutcomeKind::StrategyProposals
            && outcome.payload.item.is_some()
        {
            let action_id = Uuid::now_v7();

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
            // The content source behind a community draft is also its batch
            // key: fifty drafts carrying one synced post are one approval,
            // not fifty.
            let mut batch_source_id: Option<Uuid> = None;
            if let Some(target_id) = community_target_id {
                match self
                    .admit_community_post(
                        &mut tx,
                        outcome,
                        producing_task.as_ref(),
                        target_id,
                        producing_template,
                    )
                    .await?
                {
                    CommunityAdmission::Admitted(source_row, source_uuid) => {
                        batch_source_id = source_uuid;
                        community_source = Some(source_row);
                    }
                    CommunityAdmission::Rejected(rejection) => {
                        drop(tx);
                        self.reject_outcome(outcome.id, &rejection.to_string())
                            .await?;
                        return Ok((None, None));
                    }
                }
            }

            let is_community_post = community_target_id.is_some();
            let is_signal_push = outcome.kind == OutcomeKind::SignalPush;
            // A channel auto-post flag used to count as the operator's
            // standing approval of every draft for that channel, so an
            // LLM-written post went out with nobody having read it. It no
            // longer does: text a model wrote waits for a person, and the flag
            // only decides whether an executor publishes that approved text
            // itself or leaves it for the operator to post by hand. See
            // `model_text_authority` for the rest of the rule.
            let draft_platform = outcome
                .payload
                .item
                .as_ref()
                .and_then(|i| i.get("platform"))
                .and_then(Value::as_str);
            if outcome.kind == OutcomeKind::SocialPost
                && community_target_id.is_none()
                && self.auto_post_platforms.permits(draft_platform)
            {
                tracing::info!(
                    outcome_id = %outcome.id,
                    platform = draft_platform.unwrap_or("unknown"),
                    "channel auto-post is on, but model-written text still waits for an operator's approval"
                );
            }

            let action_details = if let Some(target_id) = community_target_id {
                Some(
                    self.community_engagement_action(
                        &mut tx,
                        outcome,
                        target_id,
                        community_source.as_ref(),
                        batch_source_id,
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
                        let Some(recipient) = press_recipient(
                            &self.pool,
                            self.workspace_id,
                            outcome.payload.item.as_ref(),
                        )
                        .await?
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
                            "template_id": "social-post",
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
                let class = parsed.action_class();
                let action_class = class.as_str();

                // Whether this may run unattended, decided here because the
                // class is decided here — on the payload, not on a literal.
                //
                // A channel flag is a standing approval the operator already
                // gave, so it short-circuits. Everything else answers to both
                // authority axes, the stricter winning, exactly as
                // `evaluate::persist` does for the brain's own candidates.
                let authority = if is_community_post || is_signal_push {
                    // The community is the target an operator can judge once.
                    // A push has no single target to have judged, so it looks
                    // for no grant and answers to the two axes alone.
                    let target_key = community_target_id.map(|id| id.to_string());
                    let strategic_review_passed = outcome
                        .payload
                        .provenance
                        .as_ref()
                        .is_some_and(|provenance| provenance.strategic_review.passed());
                    model_text_authority(
                        self.may_auto_execute(
                            &mut tx,
                            effective_context,
                            class,
                            action_kind,
                            target_key.as_deref(),
                            OffsetDateTime::now_utc(),
                        )
                        .await?,
                        strategic_review_passed,
                    )
                } else {
                    // Press pitches and everything unrecognised: a person
                    // decides. An outcome kind nobody has classified is
                    // exactly the case where asking is the right default.
                    UnattendedAuthority::Denied
                };
                let auto_execute = authority != UnattendedAuthority::Denied;
                // The workspace's own standing configuration — a channel flag
                // or the authority axes — answers for every target a piece of
                // content was drafted into. A standing grant answers for this
                // one community only: it queues this delivery and says
                // nothing about the rest of the spread.
                let spread_answered = authority == UnattendedAuthority::Policy;
                let grant_covered = authority == UnattendedAuthority::Grant;

                // ── The relay batch is the unit of approval ──
                //
                // One synced post drafted for fifty communities is one
                // question, not fifty cards. The batch row is the standing
                // answer: a draft whose content the operator already approved
                // queues into the drip directly; one whose batch was revoked
                // or already observed is discarded rather than parked; one
                // whose batch is still waiting joins the parked set the
                // single card covers.
                //
                // Two kinds of "yes" read differently here. `spread_answered`
                // — the workspace's standing policy — speaks for the whole
                // spread, so it flips the batch for every draft of this
                // content. A standing grant speaks for this community alone,
                // so it queues this delivery while the card keeps asking
                // about the rest. And a revoked batch is a veto on the
                // content itself, which beats either standing answer: the
                // grant was given for the community, not for this post.
                let mut batch_open = false;
                let mut card_approved = false;
                if let Some(source_id) = batch_source_id {
                    let created = sqlx::query_scalar::<_, String>(
                        r#"
                        INSERT INTO community_relay_batches (workspace_id, source_id)
                        VALUES ($1, $2)
                        ON CONFLICT (workspace_id, source_id) DO NOTHING
                        RETURNING status
                        "#,
                    )
                    .bind(outcome.workspace_id)
                    .bind(source_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                    let batch_status = match created {
                        Some(status) => status,
                        None => {
                            // FOR UPDATE serializes this draft against an
                            // approve/revoke landing mid-transaction: a card
                            // answer waits for this read, and this read waits
                            // for a card answer — never a parked action that
                            // the release UPDATE already missed, and never a
                            // flip that re-opens a revoked spread.
                            sqlx::query_scalar::<_, String>(
                                "SELECT status FROM community_relay_batches \
                                 WHERE workspace_id = $1 AND source_id = $2 \
                                 FOR UPDATE",
                            )
                            .bind(outcome.workspace_id)
                            .bind(source_id)
                            .fetch_one(&mut *tx)
                            .await?
                        }
                    };
                    match batch_status.as_str() {
                        "revoked" | "done" => {
                            let rejection = OutcomeRejection::RelayBatchClosed {
                                source_id,
                                status: batch_status,
                            };
                            tracing::info!(
                                outcome_id = %outcome.id,
                                rejection = %rejection,
                                "discarding community draft — its relay batch was already answered"
                            );
                            drop(tx);
                            self.reject_outcome(outcome.id, &rejection.to_string())
                                .await?;
                            return Ok((None, None));
                        }
                        // The workspace's standing policy already answered
                        // this spread for every community alike — record the
                        // answer on the batch so the drip and the card agree
                        // about who said yes, and so drafts still landing
                        // queue under it.
                        "awaiting_approval" if spread_answered => {
                            sqlx::query(
                                r#"
                                UPDATE community_relay_batches
                                SET status = 'approved',
                                    approved_at = now(),
                                    approved_by = 'policy:bounded_auto',
                                    observe_until = now() + INTERVAL '7 days',
                                    updated_at = now()
                                WHERE workspace_id = $1 AND source_id = $2
                                  AND status = 'awaiting_approval'
                                "#,
                            )
                            .bind(outcome.workspace_id)
                            .bind(source_id)
                            .execute(&mut *tx)
                            .await?;
                            batch_open = true;
                        }
                        "approved" => {
                            batch_open = true;
                            card_approved = true;
                        }
                        _ => {}
                    }
                }

                // `batch_open` queues beside `auto_execute`: the operator
                // approved the content once at the batch, so a draft landing
                // afterwards is already answered work — a second parked card
                // for it is the flood the batch exists to end.
                if auto_execute || batch_open {
                    // The attribution names the standing answer that carried
                    // this delivery: the card a person approved, the grant
                    // that covers this one community, or the workspace
                    // policy that speaks for every target.
                    let approved_by = if card_approved {
                        "operator:community_relay"
                    } else if grant_covered {
                        "standing_grant"
                    } else {
                        "policy:bounded_auto"
                    };
                    // The batch card is the approval, not a shortcut past the
                    // hold that makes revoking meaningful: a draft queuing
                    // under the card's "yes" waits its class's window exactly
                    // like one the ladder released. Standing answers — a
                    // grant, a channel flag, bounded-auto — keep the
                    // available_at they always had: no fresh decision was
                    // just made that a window could still regret.
                    sqlx::query_scalar::<_, Uuid>(
                        r#"
                    INSERT INTO autopilot_actions (
                        id, workspace_id, decision_id, context, action_kind,
                        subject_kind, subject_id, idempotency_key, payload, status,
                        action_class, approved_at, approved_by, approval_expires_at,
                        trace_id, causation_id, available_at
                    )
                    VALUES (
                        $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
                        now(), $13, NULL,
                        $12, NULL,
                        now() + make_interval(secs => $14::double precision)
                    )
                    ON CONFLICT DO NOTHING
                    RETURNING id
                    "#,
                    )
                    .bind(action_id)
                    .bind(outcome.workspace_id)
                    .bind(decision_id)
                    .bind(effective_context)
                    .bind(action_kind)
                    .bind("agent_outcome")
                    .bind(outcome.id)
                    .bind(&outcome.idempotency_key)
                    .bind(&payload)
                    .bind("queued")
                    .bind(action_class)
                    .bind(trace_id)
                    .bind(approved_by)
                    .bind(if card_approved && !auto_execute {
                        parsed.action_class().hold_seconds() as f64
                    } else {
                        0.0
                    })
                    .fetch_optional(&mut *tx)
                    .await?
                } else {
                    let inserted = sqlx::query_scalar::<_, Uuid>(
                        r#"
                    INSERT INTO autopilot_actions (
                        id, workspace_id, decision_id, context, action_kind,
                        subject_kind, subject_id, idempotency_key, payload, status,
                        action_class, approved_at, approved_by, approval_expires_at,
                        trace_id, causation_id
                    )
                    VALUES (
                        $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,
                        NULL, NULL,
                        -- A batched delivery has no per-action expiry: the
                        -- batch card is the standing question and its rows
                        -- live as long as it does. A 72h expiry here would
                        -- silently kill a spread whose card still says
                        -- "waiting on you".
                        CASE WHEN $13::bool THEN NULL
                             ELSE now() + INTERVAL '72 hours' END,
                        $12, NULL
                    )
                    ON CONFLICT DO NOTHING
                    RETURNING id
                    "#,
                    )
                    .bind(action_id)
                    .bind(outcome.workspace_id)
                    .bind(decision_id)
                    .bind(effective_context)
                    .bind(action_kind)
                    .bind("agent_outcome")
                    .bind(outcome.id)
                    .bind(&outcome.idempotency_key)
                    .bind(&payload)
                    .bind("awaiting_approval")
                    .bind(action_class)
                    .bind(trace_id)
                    .bind(batch_source_id.is_some())
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
                    // One notification per batch, not one per community it
                    // lands in — and claimed by the first draft that *parks*,
                    // not the one that created the row: a batch born from a
                    // draft that queued on a standing grant had nothing to
                    // ask yet, and a draft parking later must still be heard.
                    // The conditional UPDATE is the claim — two drafts racing
                    // in cannot both take it.
                    let notify = match batch_source_id {
                        Some(source_id) => sqlx::query_scalar::<_, Uuid>(
                            "UPDATE community_relay_batches \
                             SET notified_at = now(), updated_at = now() \
                             WHERE workspace_id = $1 AND source_id = $2 \
                               AND status = 'awaiting_approval' \
                               AND notified_at IS NULL \
                             RETURNING source_id",
                        )
                        .bind(outcome.workspace_id)
                        .bind(source_id)
                        .fetch_optional(&mut *tx)
                        .await?
                        .is_some(),
                        None => true,
                    };
                    if let (Some(inserted_id), true) = (inserted, notify) {
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
                                    'approval_expires_at',
                                        CASE WHEN $11::bool THEN NULL
                                             ELSE now() + INTERVAL '72 hours' END,
                                    'relay_batch', $11::bool,
                                    'source_id', $12::uuid,
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
                        .bind(effective_context)
                        .bind(action_kind)
                        .bind("agent_outcome")
                        .bind(outcome.id)
                        .bind(decision_reason)
                        .bind(evidence_confidence_basis_points(outcome))
                        .bind(trace_id)
                        .bind(decision_id)
                        .bind(batch_source_id.is_some())
                        .bind(batch_source_id)
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
                    "SELECT id FROM autopilot_actions \
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
                trace_id = COALESCE(trace_id, $4),
                -- The live content-hash dedup window ends at adjudication:
                -- agents/outcomes.ts documents that a topic re-emits the
                -- moment consumed_at is set, and mark_insights_consumed owns
                -- the stamp for the three insight kinds the brain reads
                -- back. For every other kind nothing ever set it, so an
                -- identical re-emission — e.g. a scout re-proposing a
                -- community that has since grown — was silently dropped
                -- forever and the readmission path could never fire.
                consumed_at = CASE
                    WHEN kind IN ('campaign_insight', 'release_plan_note', 'generic_insight')
                    THEN consumed_at
                    ELSE now()
                END
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
    /// Returns the public redirect path (`/l/{slug}`) or `None` if neither
    /// the item nor its registered source supplies a usable destination.
    async fn ensure_agent_smart_link(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        outcome: &ValidatedOutcome,
        request: AgentSmartLinkRequest<'_>,
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
            request.destination,
            &[self.public_origin.as_str()],
            request.source_canonical_url,
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

        // A failed INSERT poisons the caller's transaction — Postgres
        // aborts it (25P02) and every later statement in `map_outcome`
        // fails with an opaque "current transaction is aborted". Rolling
        // back to the savepoint keeps the tx alive so the caller's
        // "post goes out untracked" fallback is actually reachable.
        sqlx::query("SAVEPOINT agent_smart_link")
            .execute(&mut **tx)
            .await
            .map_err(AgentOutcomeError::from)?;

        let inserted = sqlx::query(
            r#"
            INSERT INTO smart_links
                (workspace_id, slug, destination_url, active,
                 channel_source, channel_community, channel_creative, campaign_id)
            VALUES ($1, $2, $3, true, $4, $5, $6, $7)
            ON CONFLICT (workspace_id, slug) DO UPDATE SET
                destination_url = EXCLUDED.destination_url,
                active = true,
                channel_source = EXCLUDED.channel_source,
                channel_community = EXCLUDED.channel_community,
                channel_creative = EXCLUDED.channel_creative,
                campaign_id = COALESCE(EXCLUDED.campaign_id, smart_links.campaign_id)
            "#,
        )
        .bind(workspace_id)
        .bind(&slug)
        .bind(destination)
        .bind(request.channel_source)
        .bind(request.channel_community)
        .bind(request.channel_creative)
        .bind(request.campaign_id)
        .execute(&mut **tx)
        .await;

        match inserted {
            Ok(_) => Ok(Some(format!("/l/{slug}"))),
            Err(error) => {
                sqlx::query("ROLLBACK TO SAVEPOINT agent_smart_link")
                    .execute(&mut **tx)
                    .await
                    .map_err(AgentOutcomeError::from)?;
                Err(AgentOutcomeError::from(error))
            }
        }
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
        producing_task: Option<&(String, String, Value)>,
    ) -> Result<(bool, Uuid), AgentOutcomeError> {
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
        // The column CHECK requires a JSON array — a scalar or object in the
        // payload is replaced by the empty array rather than sinking the
        // outcome on a constraint violation.
        let evidence = item
            .get("evidence_urls")
            .cloned()
            .filter(|e| e.is_array())
            .unwrap_or(json!([]));
        // Non-community kinds keep the field as-is — it is not their
        // identity and nothing reads it there. Community rows store the
        // normalized slug from `identity`. Bounded to the column's CHECK,
        // and a whitespace-only value is NULL — `btrim(subreddit) <> ''`
        // would sink the whole outcome on a blank string.
        let subreddit_field = item
            .get("subreddit")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(100).collect::<String>());
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
        // Every free-text field below has a column CHECK; the payload is
        // agent-controlled, so each is bounded to its column's shape here —
        // a violation would roll back the whole outcome, taking sibling
        // targets down with it. Truncation beats rejection for names and
        // contacts: the dedup keys are what they are.
        let display_name: String = display_name.chars().take(200).collect();
        let contact_email = contact_email.map(|e| e.chars().take(320).collect::<String>());
        let contact_domain = contact_domain.map(|d| d.chars().take(200).collect::<String>());
        let is_community = target_kind == "community";
        // A community's identity is its subreddit or, off Reddit, its URL —
        // normalized in community_ingestion.rs into the exact triple the row
        // stores (one dedup index per community, platform 'reddit' whenever
        // a subreddit won). Non-community kinds carry none of these fields.
        let identity = community_identity(item);
        if is_community {
            let Some((_, _, task_metadata)) = producing_task else {
                return Err(OutcomeRejection::UngroundedCommunityTarget {
                    reason: "producing task is unavailable".to_owned(),
                }
                .into());
            };
            let task_urls = evidence_urls(task_metadata);
            let identity_url = if let Some(subreddit) = identity.subreddit.as_deref() {
                Some(format!("https://www.reddit.com/r/{subreddit}"))
            } else {
                identity.community_url.clone()
            };
            let grounded = identity_url.as_deref().is_some_and(|url| {
                let direct = normalize_evidence_url(url);
                if task_urls.contains_key(&direct) {
                    return true;
                }
                let canonical = canonical_place_url(url);
                task_urls
                    .keys()
                    .any(|seen| canonical_place_url(seen) == canonical)
            });
            if !grounded {
                return Err(OutcomeRejection::UngroundedCommunityTarget {
                    reason: "community identity URL/subreddit is not among the URLs this task's evidence showed the model".to_owned(),
                }
                .into());
            }
        }
        let (place_id, verdict, refusal) = if is_community {
            self.screen_community_target(
                tx,
                outcome.workspace_id,
                &display_name,
                &identity,
                &evidence,
                language.as_deref(),
            )
            .await?
        } else {
            (None, None, None)
        };
        // A refused community is not promoted: 'proposed' + 'refused' is the
        // shape community_promotion writes for the same verdict, and the
        // growth loop's promoted+admitted filter never sees it either way.
        let initial_status = if is_community && verdict == Some("admitted") {
            "promoted"
        } else {
            "proposed"
        };
        let subreddit = if is_community {
            identity.subreddit.as_deref()
        } else {
            subreddit_field.as_deref()
        };
        // Platform and community_url only mean something on a community
        // row — a stray field on a personal contact stores nothing (and an
        // oversized one would otherwise meet the 512-char CHECK).
        let community_url = if is_community {
            identity.community_url.as_deref()
        } else {
            None
        };
        let platform = if is_community {
            identity.platform.as_deref()
        } else {
            None
        };
        // A community's identity is its subreddit or, off Reddit, its URL —
        // not its display name. The scanner may name the same sub
        // "r/deathcore" one week and "Deathcore — news & discussion" the
        // next, and display-name dedup let both live as separate promoted
        // targets (eleven subreddits sat doubled in production, each drafted
        // and posted to twice per wave). The identity upserts keep one row
        // per community: a re-proposal lands on the existing row and is
        // re-screened there, with status sticky so a discarded community
        // does not resurrect. Personal-contact kinds keep display-name
        // dedup — a person and a place do not share an identity.
        let subreddit_identity = is_community && subreddit.is_some();
        let url_identity = is_community && !subreddit_identity && community_url.is_some();
        // The three statements share one SET-list: identity is the only
        // thing that differs.
        let shared_update = r#"
            DO UPDATE SET
                subreddit = COALESCE(EXCLUDED.subreddit, agent_outreach_targets.subreddit),
                platform = COALESCE(EXCLUDED.platform, agent_outreach_targets.platform),
                community_url = COALESCE(EXCLUDED.community_url, agent_outreach_targets.community_url),
                language = COALESCE(EXCLUDED.language, agent_outreach_targets.language),
                -- Discarded is sticky, admitted promotes, refused demotes
                -- (migration 0375). Personal contacts carry no verdict and
                -- keep their status.
                status = community_target_status(
                    agent_outreach_targets.status, EXCLUDED.screening_verdict),
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
            "#;
        // The community_url a row stores is the same normalized form the
        // dedup index sees — both sides of the upsert agree on identity.
        let columns = r#"
            INSERT INTO agent_outreach_targets
                (workspace_id, target_kind, display_name, contact_email, contact_domain,
                 why_fit, evidence, source_task_id, subreddit, status,
                 place_id, screening_verdict, refusal_reason, screened_at, language,
                 platform, community_url)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,
                    CASE WHEN $2::text = 'community' THEN normalize_subreddit($9) ELSE $9 END,
                    $10,$11,$12,$13,
                    CASE WHEN $12::text IS NULL THEN NULL ELSE now() END, $14,
                    $15,
                    CASE WHEN $16::text IS NULL THEN NULL
                         ELSE normalize_community_url($16) END)
            "#;
        let sql = if subreddit_identity {
            format!(
                "{columns}
                ON CONFLICT (workspace_id, normalize_subreddit(subreddit))
                    WHERE target_kind = 'community'
                      AND subreddit IS NOT NULL
                      AND normalize_subreddit(subreddit) <> ''
                {shared_update}
                RETURNING id"
            )
        } else if url_identity {
            format!(
                "{columns}
                ON CONFLICT (workspace_id, normalize_community_url(community_url))
                    WHERE target_kind = 'community'
                      AND community_url IS NOT NULL
                      AND btrim(community_url) <> ''
                {shared_update}
                RETURNING id"
            )
        } else {
            // The name key is partial since 0350: identity-carrying
            // communities left it for the subreddit/URL indexes, so the
            // arbiter predicate must match the index's — personal contacts
            // and identity-less communities still dedupe on the name.
            format!(
                "{columns}
                ON CONFLICT (workspace_id, display_name, target_kind)
                    WHERE target_kind <> 'community'
                       OR (COALESCE(normalize_subreddit(subreddit), '') = ''
                           AND COALESCE(normalize_community_url(community_url), '') = '')
                {shared_update}
                RETURNING id"
            )
        };
        let target_id = sqlx::query_scalar::<_, Uuid>(&sql)
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
            .bind(platform)
            .bind(community_url)
            .fetch_one(&mut **tx)
            .await?;
        // Community targets are auto-promoted — the operator does not need
        // to approve them. Personal-contact kinds keep the proposed → promoted
        // operator-approval flow and need an action row. The row id goes back
        // so a strategy proposal's verdict can name what it created.
        Ok((is_community, target_id))
    }

    async fn reject_outcome(
        &self,
        outcome_id: Uuid,
        reason: &str,
    ) -> Result<(), AgentOutcomeError> {
        sqlx::query(
            r#"
            UPDATE agent_outcomes
            SET status = 'rejected', rejection_reason = $3,
                -- Rejected is terminal too: nothing reads it back, so its
                -- content hash releases here rather than suppressing an
                -- identical re-emission forever (see the processed UPDATE).
                consumed_at = now()
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
    created_at: time::OffsetDateTime,
}

/// What one draft asked for when its link gets minted: the proposed
/// destination, the channel labels the `smart_links` row records, and the
/// registered source's own URL — the one foreign destination the domain may
/// accept, because it was written by the watcher, not the model.
struct AgentSmartLinkRequest<'a> {
    destination: &'a str,
    campaign_id: Option<Uuid>,
    channel_source: &'a str,
    channel_community: Option<&'a str>,
    channel_creative: Option<&'a str>,
    source_canonical_url: Option<&'a str>,
}

#[cfg(test)]
mod tests;
