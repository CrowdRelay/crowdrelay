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
//! `screen_community_candidate` and both use the same
//! `(workspace_id, display_name, target_kind)` conflict key, so whichever
//! sees a community first wins the row and the other updates it.
//!
//! Idempotency: `agent_outcomes.idempotency_key` is unique per
//! (workspace_id, key), and the autopilot decision_key mirrors it, so worker
//! retries and task re-runs can never double-create decisions.

use std::time::Duration;

use crowdrelay_application::agent_outcomes::{OutcomeKind, ValidatedOutcome, validate};
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::target_discovery::{
    CommunityCandidateSnapshot, ScreeningVerdict, TargetDiscoveryPolicy, screen_community_candidate,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

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
enum AgentOutcomeError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("validation error: {0}")]
    Validation(#[from] crowdrelay_application::agent_outcomes::OutcomeValidationError),
    #[error("agent proposed target_kind {0:?}, which agent_outreach_targets does not accept")]
    UnknownTargetKind(String),
}

/// Row shape of the audience-graph lookup for a proposed community.
type CommunityPlaceRow = (Uuid, Option<i32>, Option<i32>, String, String, Option<i16>);

/// What the audience graph already knows about a proposed community.
#[derive(Clone, Debug)]
struct CommunityPlace {
    id: Uuid,
    member_count: Option<i32>,
    activity_bp: Option<i32>,
    status: String,
    membership_state: String,
    self_promo_ratio_percent: Option<i16>,
}

/// Builds the screening snapshot for a proposed community from the agent's
/// evidence and whatever the audience graph has measured.
///
/// Reddit places are never sold placement through this path — the discovery
/// adapters import public subreddits, not sponsorship inventory — so
/// `sells_placement` stays false rather than being guessed from prose.
fn community_snapshot(
    evidence: &Value,
    place: Option<&CommunityPlace>,
) -> CommunityCandidateSnapshot {
    let has_evidence = evidence
        .as_array()
        .is_some_and(|items| items.iter().any(|item| !item.is_null()));
    let mut snapshot = CommunityCandidateSnapshot {
        has_evidence,
        ..CommunityCandidateSnapshot::default()
    };
    if let Some(place) = place {
        snapshot.member_count = place.member_count.and_then(|v| u32::try_from(v).ok());
        snapshot.activity_basis_points = place.activity_bp.and_then(|v| u16::try_from(v).ok());
        snapshot.self_promo_ratio_percent = place
            .self_promo_ratio_percent
            .and_then(|v| u8::try_from(v).ok());
        snapshot.refused_by_us_or_them = place.status == "blocked"
            || matches!(place.membership_state.as_str(), "rejected" | "not_a_fit");
    }
    snapshot
}

#[derive(Clone, Debug)]
pub struct AgentOutcomeWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
}

impl AgentOutcomeWorker {
    #[must_use]
    pub fn new(
        pool: PgPool,
        workspace_id: WorkspaceId,
        poll_interval: Duration,
        operation_timeout: Duration,
    ) -> Self {
        Self {
            pool,
            workspace_id,
            poll_interval,
            operation_timeout,
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

    async fn run_once(&self) -> Result<usize, AgentOutcomeError> {
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

    /// Maps a validated outcome into autopilot decision (+ action) rows and
    /// any side tables (fan_segments, outreach_targets) in one transaction.
    async fn map_outcome(
        &self,
        outcome: &ValidatedOutcome,
    ) -> Result<(Option<Uuid>, Option<Uuid>), AgentOutcomeError> {
        let mut tx = self.pool.begin().await?;
        let decision_id = Uuid::now_v7();
        let input_snapshot = json!({
            "task_id": outcome.task_id,
            "result_id": outcome.result_id,
            "schema_version": outcome.schema_version,
            "payload": outcome.payload,
        });

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
        .bind("agent_outcome")
        .bind(outcome.id)
        .bind(outcome.kind.decision_kind())
        .bind(outcome.confidence_basis_points)
        .bind(outcome.kind.disposition())
        .bind({
            // The autopilot_decisions.reason column has a CHECK constraint
            // (non-empty, <=240 chars). The LLM rationale can be longer, so
            // truncate to fit. Use char-based truncation (not byte-based)
            // so multi-byte UTF-8 (Polish diacritics, emoji) doesn't
            // exceed the char_length CHECK. The full rationale is
            // preserved in input_snapshot.payload.rationale.
            let r = &outcome.payload.rationale;
            if r.chars().count() <= 240 {
                r.as_str()
            } else {
                let byte_end = r.char_indices().nth(240).map_or(r.len(), |(b, _)| b);
                // Safety: char_indices always lands on a UTF-8 boundary.
                r.get(..byte_end).unwrap_or(r)
            }
        })
        .bind(&input_snapshot)
        .bind(json!({ "source": "agent_outcome", "schema_version": outcome.schema_version }))
        .bind(json!({}))
        .bind(outcome.trace_id)
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
        match outcome.kind {
            OutcomeKind::AudienceSegments => {
                if let Some(item) = &outcome.payload.item {
                    self.insert_fan_segment(&mut tx, outcome, item).await?;
                }
            }
            OutcomeKind::OutreachTargets => {
                if let Some(item) = &outcome.payload.item {
                    self.insert_outreach_target(&mut tx, outcome, item).await?;
                }
            }
            _ => {}
        }

        // Action row only for require_approval kinds.
        let action_id = if outcome.kind.disposition() == "require_approval" {
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
            let auto_execute = if is_reddit_community_post {
                self.is_context_bounded_auto(&mut tx, "promotion_budget")
                    .await?
            } else if is_signal_push {
                self.is_context_bounded_auto(&mut tx, "fan_lifecycle")
                    .await?
            } else {
                false
            };

            let (payload, action_kind, action_class) = if let Some(target_id) = community_target_id
            {
                let item = outcome.payload.item.as_ref();
                let subreddit = item
                    .and_then(|i| i.get("subreddit"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let raw_link = item
                    .and_then(|i| i.get("smart_link"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Create a tracked smart link for attribution so we can
                // measure which Reddit posts drive ticket sales / signups.
                // Log errors instead of silently swallowing them — a post
                // without attribution is still deliverable, but the operator
                // should know the smart link failed.
                let tracked_link = if !raw_link.is_empty() {
                    match self
                        .ensure_agent_smart_link(
                            &mut tx,
                            outcome.workspace_id,
                            outcome,
                            raw_link,
                            "reddit",
                            Some(subreddit),
                        )
                        .await
                    {
                        Ok(link) => link,
                        Err(error) => {
                            tracing::warn!(
                                outcome_id = %outcome.id,
                                error = %error,
                                "failed to create agent smart link — post will go out untracked"
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                (
                    json!({
                        "kind": "request_community_engagement",
                        "target_id": target_id,
                        "platform": "reddit",
                        "subreddit": subreddit,
                        "title": item.and_then(|i| i.get("title")).and_then(Value::as_str).unwrap_or(""),
                        "body": item.and_then(|i| i.get("body")).and_then(Value::as_str).unwrap_or(""),
                        "smart_link": tracked_link,
                    }),
                    "community.engage.request",
                    "third_party",
                )
            } else {
                match outcome.kind {
                    OutcomeKind::PressPitch | OutcomeKind::SocialPost => (
                        json!({
                            "kind": "request_agent_content",
                            "task_id": outcome.task_id,
                            "draft": outcome.payload.item.clone().unwrap_or(Value::Null),
                        }),
                        "agent.content.request",
                        "first_party_reversible",
                    ),
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
                        (
                            json!({
                                "kind": "request_signal_push",
                                "task_id": outcome.task_id,
                                "title": title,
                                "body": body,
                                "target_path": target_path,
                                "event_id": event_id,
                                "segment": segment,
                            }),
                            "signal.push.request",
                            "owned_audience",
                        )
                    }
                    OutcomeKind::OutreachTargets => (
                        json!({
                            "kind": "request_agent_content",
                            "task_id": outcome.task_id,
                            "draft": outcome.payload.item.clone().unwrap_or(Value::Null),
                        }),
                        "outreach.request",
                        "first_party_reversible",
                    ),
                    _ => (
                        Value::Null,
                        "agent.content.request",
                        "first_party_reversible",
                    ),
                }
            };
            let inserted_action = if auto_execute {
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
                .bind(outcome.trace_id)
                .fetch_optional(&mut *tx)
                .await?
            } else {
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
                .bind(outcome.trace_id)
                .fetch_optional(&mut *tx)
                .await?
            };
            tracing::debug!(
                outcome_id = %outcome.id,
                idempotency_key = %outcome.idempotency_key,
                action_inserted = inserted_action.is_some(),
                "action INSERT result"
            );
            // On a re-run the action row already exists (conflict). Resolve
            // the existing id so processed_action_id is not NULL.
            match inserted_action {
                Some(id) => Some(id),
                None => {
                    sqlx::query_scalar::<_, Option<Uuid>>(
                        "SELECT id FROM viryaos_autopilot_actions \
                 WHERE workspace_id = $1 AND idempotency_key = $2",
                    )
                    .bind(outcome.workspace_id)
                    .bind(&outcome.idempotency_key)
                    .fetch_one(&mut *tx)
                    .await?
                }
            }
        } else {
            None
        };

        // Mark the outcome processed in the same transaction so a crash
        // between the decision/action inserts and the status update can
        // never leave the row stuck in 'processing' (which the poll query
        // never re-selects).
        sqlx::query(
            r#"
            UPDATE agent_outcomes
            SET status = 'processed',
                processed_decision_id = $2,
                processed_action_id = $3,
                processed_at = now()
            WHERE id = $1
            "#,
        )
        .bind(outcome.id)
        .bind(decision_id)
        .bind(action_id)
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
        if !destination.starts_with("http") {
            return Ok(None);
        }
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
                active = true,
                version = smart_links.version + 1
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
    ) -> Result<(), AgentOutcomeError> {
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
            .unwrap_or("Unnamed target");
        let contact_email = item.get("contact_email").and_then(Value::as_str);
        let contact_domain = item.get("contact_domain").and_then(Value::as_str);
        let why_fit = item.get("why_fit").and_then(Value::as_str).unwrap_or("");
        let evidence = item.get("evidence_urls").cloned().unwrap_or(json!([]));
        let subreddit = item.get("subreddit").and_then(Value::as_str);
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
            let place = self
                .community_place(tx, outcome.workspace_id, subreddit)
                .await?;
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
        sqlx::query(
            r#"
            INSERT INTO agent_outreach_targets
                (workspace_id, target_kind, display_name, contact_email, contact_domain,
                 why_fit, evidence, source_task_id, subreddit, status,
                 place_id, screening_verdict, refusal_reason, screened_at)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,
                    CASE WHEN $12::text IS NULL THEN NULL ELSE now() END)
            ON CONFLICT (workspace_id, display_name, target_kind) DO UPDATE SET
                subreddit = COALESCE(EXCLUDED.subreddit, agent_outreach_targets.subreddit),
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
            "#,
        )
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
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Looks up the audience-graph place for a proposed community, matching
    /// on the subreddit slug in the place URL. Returns `None` when discovery
    /// has not seen the community yet — that is common for a fresh proposal
    /// and is not a refusal.
    async fn community_place(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        workspace_id: Uuid,
        subreddit: Option<&str>,
    ) -> Result<Option<CommunityPlace>, AgentOutcomeError> {
        let Some(subreddit) = subreddit.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let row: Option<CommunityPlaceRow> = sqlx::query_as(
            r#"
                SELECT place.id, place.member_count, place.activity_bp,
                       place.status, place.membership_state,
                       rules.self_promo_ratio_percent
                FROM discovery_places AS place
                LEFT JOIN discovery_place_rules AS rules ON rules.place_id = place.id
                WHERE place.workspace_id = $1
                  AND place.place_kind = 'subreddit'
                  AND lower(substring(place.url from '/r/([^/?#]+)')) = lower($2)
                ORDER BY place.updated_at DESC
                LIMIT 1
                "#,
        )
        .bind(workspace_id)
        .bind(subreddit)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(row.map(
            |(id, member_count, activity_bp, status, membership_state, self_promo)| {
                CommunityPlace {
                    id,
                    member_count,
                    activity_bp,
                    status,
                    membership_state,
                    self_promo_ratio_percent: self_promo,
                }
            },
        ))
    }

    async fn reject_outcome(
        &self,
        outcome_id: Uuid,
        reason: &str,
    ) -> Result<(), AgentOutcomeError> {
        sqlx::query(
            r#"
            UPDATE agent_outcomes
            SET status = 'rejected', rejection_reason = $2
            WHERE id = $1
            "#,
        )
        .bind(outcome_id)
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
