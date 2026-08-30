//! Receipt reconciliation: closes the execution-truth gap between
//! dispatch-confirmed and provider-confirmed actions.
//!
//! `actions_execution.rs` marks an external action `succeeded` when the
//! outbox event is emitted — before any executor has done the work. The
//! authoritative completion edge is the executor receipt filed to
//! `/v1/internal/autopilot/actions/{id}/execution-report`; learning and
//! outcome evidence is only committed when that receipt arrives
//! (`record_execution_report`). Nothing else in the system compares the
//! two, so a receipt lost to an executor crash or an API outage leaves the
//! action looking `succeeded` with no provider evidence and no learning
//! sample — silently, forever (the 2026-08-29 edge outage produced exactly
//! this: three actions whose receipts had to be re-inserted by hand).
//!
//! This worker runs four sweeps per cycle:
//!
//! 1. **Gap detection.** Actions that were dispatched to an executor,
//!    finished (dispatch-wise) more than [`RECEIPT_GAP_THRESHOLD`] ago,
//!    and still have no terminal (`succeeded`/`failed`) receipt are
//!    transitioned to `unknown`. `unknown` is not a failure: the
//!    intervention may have happened, and the 7-day delayed-receipt
//!    acceptance window may still deliver one.
//!
//! 2a. **Resolution from receipts.** Actions already in `unknown` are
//!     resolved from a late-arriving terminal receipt:
//!     - `succeeded` receipt → `succeeded`/`executed`;
//!     - `failed` receipt → `failed`/`failed`.
//!
//! 2b. **Resolution from community_posts.** `community.engage.request`
//!     actions never file executor receipts — the community executor *is*
//!     the executor, and `community_posts` is its receipt:
//!     - `posted` means the Reddit post exists (succeeded);
//!     - a non-crash `failed` means the post definitively
//!       never went out (failed), and a crash-marked `failed` stays
//!       `unknown` because only a human looking at Reddit can tell.
//!
//! 2c. **Resolution from outbox delivery.** Actions that entered
//!     `unknown` because of ambiguous outbox delivery (transport timeout
//!     after max attempts) are reconciled against the outbox's own
//!     delivery status — the external truth of whether the webhook was
//!     eventually delivered:
//!     - outbox event delivered → `succeeded`;
//!     - outbox event dead with permanent rejection → `failed`;
//!     - outbox event dead with ambiguous error → stays `unknown`.
//!
//! Experiment assignments follow the action through every transition so
//! the causal learner never counts an unresolved intervention as either
//! treatment or failure. The ops watchdog alerts on the remaining
//! `unknown` population (`execution.unknown_outcome`).
//!
//! Note: `record_execution_report` (in `runtime.rs`) handles the normal
//! success path — when a terminal receipt arrives on time, it transitions
//! the assignment `dispatched → executed/failed` immediately. This worker
//! only handles the gap/late/unknown paths.

use std::time::Duration;

use crate::community_executor::CRASH_POSTING_ERROR_PREFIX;
use crowdrelay_application::autopilot::AutopilotActionPayload;
use crowdrelay_domain::WorkspaceId;
use crowdrelay_domain::action_ledger::{
    ActionState, LegalTransition, ProviderDeliveryState, ResolutionEvidence, legal_transition,
    resolve_observation,
};
use crowdrelay_infra::autopilot::payload_requires_executor;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use thiserror::Error;
use tokio::{
    sync::watch,
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

/// How long after dispatch an external action may sit `succeeded` without
/// a terminal receipt before it is treated as a lost receipt. This measures
/// **dispatch_age** (time since `finished_at` was set by `actions_execution.rs`
/// when the action was dispatch-confirmed), NOT **unknown_age** (time since
/// the action entered the `unknown` state). The gap sweep only operates on
/// `status = 'succeeded'` actions, where `finished_at` is guaranteed non-NULL
/// by the table's CHECK constraint.
///
/// Chosen well under the API's 7-day delayed-receipt acceptance window: a
/// late receipt arriving after the transition still resolves the action
/// back via sweep 2, because the receipt handler records evidence
/// regardless of the action's status.
///
/// The watchdog's `execution.unknown_outcome` alert uses a separate
/// `unknown_age` clock (from `action_ledger.state_entered_at` when
/// `state = 'UNKNOWN'`) — see `ops_watchdog.rs`.
const RECEIPT_GAP_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// Upper bound on rows examined per phase per cycle, so a large backlog
/// drains progressively instead of stretching one transaction.
const SWEEP_BATCH_LIMIT: i64 = 500;

#[derive(Debug, Error)]
enum ReceiptReconciliationError {
    #[error("receipt reconciliation database operation failed")]
    Database(#[from] sqlx::Error),
}

#[derive(Clone)]
pub struct ReceiptReconciliationWorker {
    pool: PgPool,
    workspace_id: WorkspaceId,
    poll_interval: Duration,
    operation_timeout: Duration,
}

impl ReceiptReconciliationWorker {
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
                        Ok(Ok(reconciled)) if reconciled > 0 => {
                            tracing::info!(reconciled, "receipt reconciliation reconciled actions");
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => tracing::warn!(error = %error, "receipt reconciliation cycle failed"),
                        Err(_) => tracing::warn!("receipt reconciliation cycle timed out"),
                    }
                }
            }
        }
    }

    /// One reconciliation cycle. Returns how many actions transitioned
    /// (into or out of `unknown`).
    async fn run_once(&self) -> Result<usize, ReceiptReconciliationError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "{}:viryaos-receipt-reconciliation",
                self.workspace_id
            ))
            .execute(&mut *transaction)
            .await?;
        let gaps = self.detect_receipt_gaps(&mut transaction).await?;
        let by_receipt = self.resolve_from_receipts(&mut transaction).await?;
        let by_community = self.resolve_community_posts(&mut transaction).await?;
        let by_outbox = self.resolve_from_outbox(&mut transaction).await?;
        transaction.commit().await?;
        Ok(gaps + by_receipt + by_community + by_outbox)
    }

    /// Sweep 1: dispatch-confirmed actions whose terminal receipt never
    /// arrived become `unknown` so the gap is visible and reconcilable
    /// instead of masquerading as provider-confirmed success.
    async fn detect_receipt_gaps(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<usize, ReceiptReconciliationError> {
        let candidates: Vec<(Uuid, Value)> = sqlx::query_as(
            r#"
            SELECT a.id, a.payload
            FROM viryaos_autopilot_actions a
            WHERE a.workspace_id = $1
              AND a.status = 'succeeded'
              AND a.finished_at < now() - make_interval(secs => $2::double precision)
              AND EXISTS (
                  SELECT 1 FROM viryaos_autopilot_action_emissions e
                  WHERE e.workspace_id = a.workspace_id AND e.action_id = a.id
              )
              AND NOT EXISTS (
                  SELECT 1 FROM viryaos_autopilot_execution_reports r
                  WHERE r.workspace_id = a.workspace_id AND r.action_id = a.id
                    AND r.status IN ('succeeded', 'failed')
              )
            ORDER BY a.finished_at
            LIMIT $3
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(RECEIPT_GAP_THRESHOLD.as_secs() as i64)
        .bind(SWEEP_BATCH_LIMIT)
        .fetch_all(&mut **transaction)
        .await?;

        let mut gapped = 0usize;
        for (action_id, payload_value) in candidates {
            // The payload decides whether an executor was ever supposed to
            // report back; kind strings alone cannot (show.growth depends
            // on the lever). Unparseable payloads are skipped loudly —
            // never silently reclassified.
            let payload = match serde_json::from_value::<AutopilotActionPayload>(payload_value) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(
                        action_id = %action_id,
                        error = %error,
                        "receipt gap: skipping action with unparseable payload"
                    );
                    continue;
                }
            };
            if !requires_terminal_receipt(&payload) {
                continue;
            }

            let transitioned = sqlx::query(
                r#"
                UPDATE viryaos_autopilot_actions
                SET status = 'unknown',
                    finished_at = NULL,
                    updated_at = now()
                WHERE workspace_id = $1
                  AND id = $2
                  AND status = 'succeeded'
                "#,
            )
            .bind(self.workspace_id.into_uuid())
            .bind(action_id)
            .execute(&mut **transaction)
            .await?
            .rows_affected();
            if transitioned == 0 {
                continue;
            }
            transition_assignment(transaction, action_id, AssignmentTransition::Unknown).await?;
            tracing::warn!(
                action_id = %action_id,
                "executor receipt missing after dispatch — action marked unknown"
            );
            gapped += 1;
        }
        Ok(gapped)
    }

    /// Sweep 2a: an `unknown` action whose terminal receipt arrived (late,
    /// after the gap sweep already ran) resolves to the receipt's verdict.
    /// The receipt handler already committed the learning evidence; this
    /// only aligns the action and assignment statuses with it.
    async fn resolve_from_receipts(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<usize, ReceiptReconciliationError> {
        // Load the latest terminal receipt per unknown action, plus
        // whether any earlier succeeded receipt exists for the same
        // action+executor. If a prior success exists, the effective
        // observation is Executed regardless of the latest receipt's
        // status — the caller adjusts the evidence before calling the
        // pure resolver. The current state is always Unknown (the query
        // filters status = 'unknown').
        let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
            r#"
            SELECT a.id, r.status, r.prior_success
            FROM viryaos_autopilot_actions a
            JOIN LATERAL (
                SELECT status,
                       EXISTS (
                           SELECT 1 FROM viryaos_autopilot_execution_reports r2
                           WHERE r2.workspace_id = a.workspace_id
                             AND r2.action_id = a.id
                             AND r2.executor_id = r.executor_id
                             AND r2.status = 'succeeded'
                             AND (r2.occurred_at, r2.id) < (r.occurred_at, r.id)
                       ) AS prior_success
                FROM viryaos_autopilot_execution_reports r
                WHERE r.workspace_id = a.workspace_id AND r.action_id = a.id
                  AND r.status IN ('succeeded', 'failed')
                ORDER BY r.occurred_at DESC, r.id DESC
                LIMIT 1
            ) r ON true
            WHERE a.workspace_id = $1
              AND a.status = 'unknown'
            LIMIT $2
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(SWEEP_BATCH_LIMIT)
        .fetch_all(&mut **transaction)
        .await?;

        let mut resolved = 0usize;
        for (action_id, receipt_status, prior_success_exists) in rows {
            // If a prior succeeded receipt exists, the effective
            // observation is Executed — regardless of the latest
            // receipt's status. This is the caller's responsibility:
            // the pure resolver (resolve_observation) knows nothing
            // about persistence-derived state. legal_transition
            // (Unknown, Executed) → Apply(Succeeded) resolves correctly.
            let evidence = if prior_success_exists {
                ResolutionEvidence::TerminalReceipt { succeeded: true }
            } else {
                ResolutionEvidence::TerminalReceipt {
                    succeeded: receipt_status == "succeeded",
                }
            };
            match legal_transition(ActionState::Unknown, resolve_observation(evidence)) {
                LegalTransition::Apply(ActionState::Succeeded) => {
                    resolved += resolve_action(
                        self.workspace_id,
                        transaction,
                        action_id,
                        ActionOutcome::Succeeded,
                    )
                    .await?;
                }
                LegalTransition::Apply(ActionState::Failed) => {
                    resolved += resolve_action(
                        self.workspace_id,
                        transaction,
                        action_id,
                        ActionOutcome::Failed,
                    )
                    .await?;
                }
                LegalTransition::NoChange => continue,
                LegalTransition::Conflict => {
                    tracing::warn!(
                        action_id = %action_id,
                        "CONFLICT: contradictory evidence for terminal action — \
                         not changing state. Investigate the contradictory evidence."
                    );
                    continue;
                }
                LegalTransition::Apply(_) => continue,
            }
        }
        Ok(resolved)
    }

    /// Sweep 2b: `community.engage.request` actions never file executor
    /// receipts — the community executor *is* the executor, and
    /// `community_posts` is its receipt. Resolve from that table.
    async fn resolve_community_posts(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<usize, ReceiptReconciliationError> {
        let rows: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
            r#"
            SELECT a.id, cp.status, cp.error_message
            FROM viryaos_autopilot_actions a
            JOIN community_posts cp ON cp.action_id = a.id
            WHERE a.workspace_id = $1
              AND a.status = 'unknown'
              AND a.action_kind = 'community.engage.request'
            LIMIT $2
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(SWEEP_BATCH_LIMIT)
        .fetch_all(&mut **transaction)
        .await?;

        let mut resolved = 0usize;
        for (action_id, post_status, error_message) in rows {
            // Use the canonical resolver via the provider-specific adapter.
            let evidence = community_post_to_evidence(&post_status, error_message.as_deref());
            match legal_transition(ActionState::Unknown, resolve_observation(evidence)) {
                LegalTransition::Apply(ActionState::Succeeded) => {
                    resolved += resolve_action(
                        self.workspace_id,
                        transaction,
                        action_id,
                        ActionOutcome::Succeeded,
                    )
                    .await?;
                }
                LegalTransition::Apply(ActionState::Failed) => {
                    resolved += resolve_action(
                        self.workspace_id,
                        transaction,
                        action_id,
                        ActionOutcome::Failed,
                    )
                    .await?;
                }
                LegalTransition::NoChange => continue,
                LegalTransition::Conflict => {
                    tracing::warn!(
                        action_id = %action_id,
                        "CONFLICT: contradictory evidence for terminal action — \
                         not changing state. Investigate the contradictory evidence."
                    );
                    continue;
                }
                LegalTransition::Apply(_) => continue,
            }
        }
        Ok(resolved)
    }

    /// Sweep 2c: resolve `unknown` actions from their outbox delivery
    /// status. This is the authoritative reconciliation for actions that
    /// entered `unknown` because of ambiguous transport failures (timeout
    /// after max attempts — the request may or may not have reached the
    /// provider).
    ///
    /// The outbox delivery status is the external truth: if the webhook
    /// was eventually delivered (by a retry from a different worker, or a
    /// late lease recovery), the action succeeded. If the outbox event is
    /// dead with a permanent rejection, the action failed. If it's dead
    /// with an ambiguous error, the action stays `unknown` — only a
    /// provider-specific reconciliation or manual check can resolve it.
    async fn resolve_from_outbox(
        &self,
        transaction: &mut Transaction<'_, Postgres>,
    ) -> Result<usize, ReceiptReconciliationError> {
        // Find unknown actions that have a linked outbox event, and check
        // the outbox event's delivery status. Skip community.engage
        // actions (handled by sweep 2b) and actions that already have
        // executor receipts (handled by sweep 2a).
        let rows: Vec<(Uuid, String, Option<String>)> = sqlx::query_as(
            r#"
            SELECT a.id, e.status, e.last_error_kind
            FROM viryaos_autopilot_actions a
            JOIN outbox_events e
                ON e.workspace_id = a.workspace_id
                AND e.action_id = a.id
            WHERE a.workspace_id = $1
              AND a.status = 'unknown'
              AND a.action_kind <> 'community.engage.request'
              AND NOT EXISTS (
                  SELECT 1 FROM viryaos_autopilot_execution_reports r
                  WHERE r.workspace_id = a.workspace_id AND r.action_id = a.id
                    AND r.status IN ('succeeded', 'failed')
              )
            LIMIT $2
            "#,
        )
        .bind(self.workspace_id.into_uuid())
        .bind(SWEEP_BATCH_LIMIT)
        .fetch_all(&mut **transaction)
        .await?;

        let mut resolved = 0usize;
        for (action_id, outbox_status, last_error_kind) in rows {
            // Route through the canonical resolver via the outbox adapter.
            let evidence = outbox_event_to_evidence(&outbox_status, last_error_kind.as_deref());
            match legal_transition(ActionState::Unknown, resolve_observation(evidence)) {
                LegalTransition::Apply(ActionState::Succeeded) => {
                    resolved += resolve_action(
                        self.workspace_id,
                        transaction,
                        action_id,
                        ActionOutcome::Succeeded,
                    )
                    .await?;
                }
                LegalTransition::Apply(ActionState::Failed) => {
                    resolved += resolve_action(
                        self.workspace_id,
                        transaction,
                        action_id,
                        ActionOutcome::Failed,
                    )
                    .await?;
                }
                LegalTransition::NoChange => continue,
                LegalTransition::Conflict => {
                    tracing::warn!(
                        action_id = %action_id,
                        "CONFLICT: contradictory evidence for terminal action — \
                         not changing state. Investigate the contradictory evidence."
                    );
                    continue;
                }
                LegalTransition::Apply(_) => continue,
            }
        }
        Ok(resolved)
    }
}

/// Whether this action's payload obliges an external executor to file a
/// terminal receipt. `community.engage.request` is the exception: it is
/// executor-required at dispatch (capability gate) but executed by the
/// internal community worker, whose `community_posts` row is the receipt.
fn requires_terminal_receipt(payload: &AutopilotActionPayload) -> bool {
    payload_requires_executor(payload)
        && !matches!(
            payload,
            AutopilotActionPayload::RequestCommunityEngagement { .. }
        )
}

#[derive(Clone, Copy)]
enum AssignmentTransition {
    Unknown,
    Executed,
    Failed,
}

impl AssignmentTransition {
    const fn status(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Executed => "executed",
            Self::Failed => "failed",
        }
    }

    /// Only these guard clauses are legal; anything else is a no-op so the
    /// sweep stays idempotent under concurrency.
    const fn requires_from(self) -> &'static str {
        match self {
            Self::Unknown => "dispatched",
            Self::Executed | Self::Failed => "unknown",
        }
    }
}

async fn transition_assignment(
    transaction: &mut Transaction<'_, Postgres>,
    action_id: Uuid,
    transition: AssignmentTransition,
) -> Result<(), ReceiptReconciliationError> {
    sqlx::query(
        r#"
        UPDATE viryaos_experiment_assignments
        SET execution_status = $1,
            trace_id = COALESCE(
                trace_id,
                (SELECT trace_id FROM viryaos_autopilot_actions WHERE id = $2)
            )
        WHERE action_id = $2
          AND execution_status = $3
          AND EXISTS (
              SELECT 1 FROM viryaos_autopilot_actions a
              WHERE a.id = $2 AND viryaos_experiment_assignments.workspace_id = a.workspace_id
          )
        "#,
    )
    .bind(transition.status())
    .bind(action_id)
    .bind(transition.requires_from())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

/// Terminal outcome to write for a resolved action.
enum ActionOutcome {
    Succeeded,
    Failed,
}

/// Moves an `unknown` action to its resolved terminal status and follows
/// the experiment assignment. Returns 1 if the action transitioned, 0 if
/// another runner resolved it first.
async fn resolve_action(
    workspace_id: WorkspaceId,
    transaction: &mut Transaction<'_, Postgres>,
    action_id: Uuid,
    outcome: ActionOutcome,
) -> Result<usize, ReceiptReconciliationError> {
    let (status, error_kind) = match outcome {
        ActionOutcome::Succeeded => ("succeeded", None),
        ActionOutcome::Failed => ("failed", Some("receipt_reconciliation")),
    };
    let transitioned = sqlx::query(
        r#"
        UPDATE viryaos_autopilot_actions
        SET status = $3,
            finished_at = now(),
            last_error_kind = $4,
            updated_at = now()
        WHERE workspace_id = $1
          AND id = $2
          AND status = 'unknown'
        "#,
    )
    .bind(workspace_id.into_uuid())
    .bind(action_id)
    .bind(status)
    .bind(error_kind)
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if transitioned == 0 {
        return Ok(0);
    }
    transition_assignment(
        transaction,
        action_id,
        if status == "succeeded" {
            AssignmentTransition::Executed
        } else {
            AssignmentTransition::Failed
        },
    )
    .await?;
    tracing::info!(
        action_id = %action_id,
        status,
        "resolved unknown action outcome"
    );
    Ok(1)
}

/// Provider-specific adapter: translates `community_posts` state into
/// canonical [`ResolutionEvidence`] facts for the
/// [`resolve_observation`] + [`legal_transition`] resolver.
///
/// This is the ONLY place that knows about `community_posts.status`
/// values and the crash marker prefix. The canonical resolver
/// (`resolve_observation` + `legal_transition`) makes the semantic
/// decision; this adapter just converts provider state into domain
/// facts.
fn community_post_to_evidence(
    post_status: &str,
    error_message: Option<&str>,
) -> ResolutionEvidence {
    match post_status {
        "posted" => ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::Confirmed),
        "failed" => {
            // The stale-posting recovery marks crash rows with this
            // prefix; those are confirmation-lost, not definitive failures.
            let crashed = error_message
                .is_some_and(|message| message.starts_with(CRASH_POSTING_ERROR_PREFIX));
            if crashed {
                ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::ConfirmationLost)
            } else {
                ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::DefinitiveFailure)
            }
        }
        // `pending`/`posting`/`rate_limited` are in-flight or awaiting
        // retry; `awaiting_manual_post` is waiting on the operator. All
        // resolve later through their own paths.
        _ => ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::InFlight),
    }
}

/// Provider-specific adapter: translates outbox event delivery state into
/// canonical [`ResolutionEvidence`] facts for the
/// [`resolve_observation`] + [`legal_transition`] resolver.
///
/// This is the ONLY place that knows about `outbox_events.status` values
/// and which error kinds are permanent vs ambiguous. The canonical
/// resolver (`resolve_observation` + `legal_transition`) makes the
/// semantic decision; this adapter just converts provider state into
/// domain facts.
fn outbox_event_to_evidence(
    outbox_status: &str,
    last_error_kind: Option<&str>,
) -> ResolutionEvidence {
    match outbox_status {
        // Outbox event was delivered → the side effect happened.
        "delivered" => ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::Confirmed),
        // Outbox event is dead → check the error kind.
        "dead" => {
            let permanent = matches!(
                last_error_kind,
                Some(kind)
                    if kind.starts_with("http_permanent")
                        || kind == "recipient_ineligible"
                        || kind.starts_with("secret_")
                        || kind.starts_with("endpoint_")
                        || kind == "invalid_signing_secret"
                        || kind == "event_serialization"
            );
            if permanent {
                // Permanent rejection (provider saw it and rejected)
                // → definitively failed.
                ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::DefinitiveFailure)
            } else {
                // Ambiguous errors (transport_timeout, transport_request,
                // lease_expired) → confirmation lost. Only external truth
                // can resolve these.
                ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::ConfirmationLost)
            }
        }
        // Pending/processing → still in flight, no evidence yet.
        _ => ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::InFlight),
    }
}

#[cfg(test)]
mod tests {
    use super::{community_post_to_evidence, outbox_event_to_evidence, requires_terminal_receipt};
    use crowdrelay_application::autopilot::AutopilotActionPayload;
    use crowdrelay_domain::FanId;
    use crowdrelay_domain::action_ledger::{
        ActionState, LegalTransition, legal_transition, resolve_observation,
    };
    use uuid::Uuid;

    fn community_payload() -> AutopilotActionPayload {
        AutopilotActionPayload::RequestCommunityEngagement {
            target_id: Uuid::nil(),
            platform: "reddit".to_owned(),
            subreddit: Some("metal".to_owned()),
            title: "t".to_owned(),
            body: "b".to_owned(),
            smart_link: None,
        }
    }

    #[test]
    fn community_engagement_does_not_require_a_receipt() {
        // The community worker's community_posts row is the receipt.
        assert!(!requires_terminal_receipt(&community_payload()));
    }

    #[test]
    fn executor_required_payload_requires_a_receipt() {
        let payload = AutopilotActionPayload::RequestFanLifecycleMessage {
            fan_id: FanId::from_uuid(Uuid::nil()),
            template_key: "k".to_owned(),
        };
        assert!(requires_terminal_receipt(&payload));
    }

    #[test]
    fn first_party_payload_does_not_require_a_receipt() {
        let payload = AutopilotActionPayload::RequestSignalPush {
            task_id: Uuid::nil(),
            title: "t".to_owned(),
            body: "b".to_owned(),
            target_path: None,
            event_id: None,
            segment: None,
        };
        assert!(!requires_terminal_receipt(&payload));
    }

    #[test]
    fn posted_community_post_resolves_succeeded() {
        let evidence = community_post_to_evidence("posted", None);
        assert_eq!(
            legal_transition(ActionState::Unknown, resolve_observation(evidence)),
            LegalTransition::Apply(ActionState::Succeeded)
        );
    }

    #[test]
    fn crash_marked_failure_stays_unknown() {
        let evidence = community_post_to_evidence(
            "failed",
            Some("worker crashed during posting — check Reddit manually"),
        );
        // ConfirmationLost → observation is Unknown. The action is already
        // Unknown, so legal_transition(Unknown, Unknown) → NoChange.
        // The action stays unknown — NoChange is correct.
        assert_eq!(
            legal_transition(ActionState::Unknown, resolve_observation(evidence)),
            LegalTransition::NoChange
        );
    }

    #[test]
    fn definitive_community_failure_resolves_failed() {
        let evidence = community_post_to_evidence("failed", Some("no agents service configured"));
        assert_eq!(
            legal_transition(ActionState::Unknown, resolve_observation(evidence)),
            LegalTransition::Apply(ActionState::Failed)
        );
        let evidence = community_post_to_evidence("failed", None);
        assert_eq!(
            legal_transition(ActionState::Unknown, resolve_observation(evidence)),
            LegalTransition::Apply(ActionState::Failed)
        );
    }

    #[test]
    fn in_flight_community_statuses_stay_unknown() {
        for status in ["pending", "posting", "rate_limited", "awaiting_manual_post"] {
            let evidence = community_post_to_evidence(status, None);
            // InFlight → observation is Unknown. The action is already
            // Unknown, so NoChange — the action stays unknown.
            assert_eq!(
                legal_transition(ActionState::Unknown, resolve_observation(evidence)),
                LegalTransition::NoChange,
                "status {status} should stay unknown (NoChange from Unknown)"
            );
        }
    }

    #[test]
    fn unrelated_failure_messages_do_not_match_crash_prefix() {
        // Errors that happen to contain "crash" or "posting" but do not
        // start with the exact crash marker prefix must be treated as
        // definitive failures, not confirmation loss.
        let evidence =
            community_post_to_evidence("failed", Some("subreddit crashed during posting"));
        assert_eq!(
            legal_transition(ActionState::Unknown, resolve_observation(evidence)),
            LegalTransition::Apply(ActionState::Failed)
        );
        let evidence = community_post_to_evidence("failed", Some("posting failed: rate limit"));
        assert_eq!(
            legal_transition(ActionState::Unknown, resolve_observation(evidence)),
            LegalTransition::Apply(ActionState::Failed)
        );
        let evidence = community_post_to_evidence("failed", Some("worker crashed during posting"));
        // Crash marker → ConfirmationLost → Unknown observation.
        // Action already Unknown → NoChange (stays unknown).
        assert_eq!(
            legal_transition(ActionState::Unknown, resolve_observation(evidence)),
            LegalTransition::NoChange
        );
    }

    // ── outbox_event_to_evidence adapter tests ──

    #[test]
    fn delivered_outbox_resolves_executed() {
        let evidence = outbox_event_to_evidence("delivered", None);
        assert_eq!(
            legal_transition(ActionState::Unknown, resolve_observation(evidence)),
            LegalTransition::Apply(ActionState::Succeeded)
        );
    }

    #[test]
    fn dead_outbox_permanent_rejection_resolves_failed() {
        for kind in [
            "http_permanent_status",
            "recipient_ineligible",
            "secret_missing",
            "endpoint_unreachable",
            "invalid_signing_secret",
            "event_serialization",
        ] {
            let evidence = outbox_event_to_evidence("dead", Some(kind));
            assert_eq!(
                legal_transition(ActionState::Unknown, resolve_observation(evidence)),
                LegalTransition::Apply(ActionState::Failed),
                "permanent error kind {kind} must resolve to Failed"
            );
        }
    }

    #[test]
    fn dead_outbox_ambiguous_error_stays_unknown() {
        for kind in ["transport_timeout", "transport_request", "lease_expired"] {
            let evidence = outbox_event_to_evidence("dead", Some(kind));
            // Ambiguous error → ConfirmationLost → Unknown observation.
            // Action already Unknown → NoChange (stays unknown).
            assert_eq!(
                legal_transition(ActionState::Unknown, resolve_observation(evidence)),
                LegalTransition::NoChange,
                "ambiguous error kind {kind} must stay unknown (NoChange from Unknown)"
            );
        }
    }

    #[test]
    fn in_flight_outbox_statuses_stay_unknown() {
        for status in ["pending", "processing", "leased"] {
            let evidence = outbox_event_to_evidence(status, None);
            // InFlight → Unknown observation. Action already Unknown →
            // NoChange (stays unknown).
            assert_eq!(
                legal_transition(ActionState::Unknown, resolve_observation(evidence)),
                LegalTransition::NoChange,
                "in-flight status {status} should stay unknown (NoChange from Unknown)"
            );
        }
    }
}
