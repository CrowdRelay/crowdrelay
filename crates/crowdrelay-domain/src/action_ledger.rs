//! Action Ledger state machine — the canonical execution state for every
//! autopilot action.
//!
//! The ledger is append-only and history-safe. The current row is a
//! projection — the full transition history is auditable through the trace
//! timeline.
//!
//! # Enforcement
//!
//! The state machine is enforced by SQL triggers in migrations 0185 and 0190
//! (`viryaos_action_ledger_sync`). Migration 0185 covers the base transitions;
//! migration 0190 adds the `unknown → UNKNOWN` and `SUCCEEDED → FAILED/UNKNOWN`
//! correction mappings. This Rust module is the domain-level documentation and
//! test surface for the same rules. The triggers and `can_transition_to` must
//! stay in sync — if you add a transition here, add it to the trigger too.
//!
//! # State transitions
//!
//! ```text
//! PLANNED     → AUTHORIZED | CANCELLED | REVOKED
//! AUTHORIZED  → QUEUED | CANCELLED | REVOKED
//! QUEUED      → RUNNING | CANCELLED | FAILED
//! RUNNING     → SUCCEEDED | FAILED | UNKNOWN
//! UNKNOWN     → RECONCILING | SUCCEEDED | FAILED
//! RECONCILING → SUCCEEDED | FAILED | UNKNOWN
//! SUCCEEDED   → FAILED | UNKNOWN  (correction of premature success)
//! FAILED      → (terminal)
//! CANCELLED   → (terminal)
//! REVOKED     → (terminal)
//! ```
//!
//! `PLANNED` and `REVOKED` are defined in the state machine but are not
//! currently reachable from any `autopilot_actions.status` value. They exist
//! for forward compatibility (planning phase, explicit revocation). No trigger
//! mapping produces them today.
//!
//! # SUCCEEDED correction
//!
//! `actions_execution.rs` marks the action `succeeded` when dispatching to
//! the executor — before the external intervention is confirmed. The
//! community executor may later correct this to `failed` (definitive
//! execution failure) or `unknown` (confirmation lost). This is the only
//! non-terminal use of `SUCCEEDED`; it is not a normal transition.
//!
//! # UNKNOWN semantics
//!
//! `UNKNOWN` means "CrowdRelay cannot establish whether the external side
//! effect happened." It is NOT a failure. Retry mechanisms must NOT treat
//! `UNKNOWN` as a failure (to avoid duplicate side effects). Instead,
//! `UNKNOWN` triggers reconciliation, which may resolve to `SUCCEEDED` or
//! `FAILED`.
//!
//! # UNKNOWN in the community executor
//!
//! When the community executor detects a stale `posting` row (worker crash
//! during the Reddit API call), it transitions the autopilot action to
//! `'unknown'` — NOT `'failed'`. The Reddit post may have actually succeeded;
//! we simply lost confirmation. The action ledger maps `'unknown'` to
//! `UNKNOWN`, and the experiment assignment is also transitioned to
//! `'unknown'`, which excludes it from both realized-treatment and
//! failed-treatment counts in the causal learner. `UNKNOWN` is non-terminal:
//! it can later resolve to `SUCCEEDED` or `FAILED` via reconciliation.
//!
//! # State-machine mapping contract (P1-1)
//!
//! CrowdRelay has four interacting state machines. They track two
//! semantically distinct layers. This contract documents the authoritative
//! relationship and prevents semantic drift.
//!
//! ## Two semantic layers
//!
//! 1. **Operational execution state** — "Did CrowdRelay's execution
//!    pipeline complete, and what is the execution certainty?"
//!    - `autopilot_actions.status` (primary)
//!    - `action_ledger.state` (projection, trigger-maintained)
//!
//! 2. **Causal treatment realization** — "Was the treatment actually
//!    realized for causal inference?"
//!    - `experiment_assignments.execution_status` (independent)
//!
//! 3. **Provider delivery state** — "What does the external provider
//!    (Reddit, email, push) say about delivery?"
//!    - `community_posts.status` (independent, provider-specific)
//!
//! ## Authority
//!
//! | System | Field | Authority |
//! |--------|-------|-----------|
//! | `autopilot_actions.status` | operational lifecycle | **Primary** |
//! | `action_ledger.state` | operational certainty | **Projection** (trigger) |
//! | `experiment_assignments.execution_status` | causal realization | **Independent** |
//! | `community_posts.status` | provider delivery | **Independent** |
//!
//! ## One-way projections (no bidirectional sync loops)
//!
//! ```text
//! autopilot_actions.status → action_ledger.state (via trigger)
//!   awaiting_approval → AUTHORIZED
//!   queued → QUEUED
//!   processing → RUNNING
//!   succeeded → SUCCEEDED (may later correct to FAILED/UNKNOWN)
//!   failed → FAILED
//!   cancelled → CANCELLED
//!   unknown → UNKNOWN
//!
//! autopilot_actions.status → experiment_assignments.execution_status (via worker/runtime)
//!   dispatched → dispatched (at assignment creation)
//!   succeeded + terminal receipt → executed (record_execution_report)
//!   failed + terminal receipt → failed (record_execution_report)
//!   unknown → unknown (detect_receipt_gaps or community executor crash)
//!   unknown + late receipt → executed/failed (resolve_from_receipts)
//!   community.engage: posted → executed (community_executor)
//!   community.engage: failed (definitive) → failed (community_executor)
//!   community.engage: failed (crash) → unknown (community_executor)
//!
//! community_posts.status → autopilot_actions.status (via worker, NOT trigger)
//!   posted → succeeded (confirmed external delivery)
//!   failed (definitive) → failed
//!   failed (crash-marked) → unknown
//!   pending/posting/rate_limited → no change (in-flight)
//! ```
//!
//! The experiment assignment execution_status is NOT a projection of the
//! action ledger — it has its own state machine (`ExecutionStatus` in
//! `experiment.rs`) with its own transition rules. They mirror each other
//! in many cases but are NOT identical. The causal learner checks
//! `execution_status`, not `action_ledger.state`, to decide whether a
//! treatment was realized.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The canonical state of an action in the Action Ledger.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionState {
    /// The action has been planned but not yet authorized.
    Planned,
    /// The action has been authorized (approved) but not yet queued.
    Authorized,
    /// The action has been queued for execution.
    Queued,
    /// The action is currently running (external execution in progress).
    Running,
    /// The action succeeded — the external side effect is confirmed.
    Succeeded,
    /// The action failed — the external side effect did not happen.
    Failed,
    /// CrowdRelay cannot establish whether the external side effect happened.
    /// NOT a failure — triggers reconciliation, not retry.
    Unknown,
    /// Reconciliation is in progress — attempting to resolve UNKNOWN.
    Reconciling,
    /// The action was cancelled before execution.
    Cancelled,
    /// The action was revoked (authorization withdrawn).
    Revoked,
}

impl ActionState {
    /// Returns the string representation used in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "PLANNED",
            Self::Authorized => "AUTHORIZED",
            Self::Queued => "QUEUED",
            Self::Running => "RUNNING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::Unknown => "UNKNOWN",
            Self::Reconciling => "RECONCILING",
            Self::Cancelled => "CANCELLED",
            Self::Revoked => "REVOKED",
        }
    }

    /// Parses a state from its string representation.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "PLANNED" => Some(Self::Planned),
            "AUTHORIZED" => Some(Self::Authorized),
            "QUEUED" => Some(Self::Queued),
            "RUNNING" => Some(Self::Running),
            "SUCCEEDED" => Some(Self::Succeeded),
            "FAILED" => Some(Self::Failed),
            "UNKNOWN" => Some(Self::Unknown),
            "RECONCILING" => Some(Self::Reconciling),
            "CANCELLED" => Some(Self::Cancelled),
            "REVOKED" => Some(Self::Revoked),
            _ => None,
        }
    }

    /// Returns true if this state is terminal (no further transitions).
    ///
    /// Note: `SUCCEEDED` is NOT terminal in the community executor path —
    /// the executor may correct a premature success to `FAILED` or `UNKNOWN`.
    /// It is terminal in all other paths.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled | Self::Revoked)
    }

    /// Returns true if this state allows further transitions.
    #[must_use]
    pub const fn is_active(self) -> bool {
        !self.is_terminal()
    }

    /// Returns true if transitioning from `self` to `target` is legal.
    ///
    /// The transition rules are monotonic — backwards transitions are
    /// rejected. The only exception is `UNKNOWN → RECONCILING → UNKNOWN`
    /// (reconciliation may not resolve on the first attempt).
    #[must_use]
    pub const fn can_transition_to(self, target: Self) -> bool {
        match (self, target) {
            // PLANNED → AUTHORIZED | CANCELLED | REVOKED
            (Self::Planned, Self::Authorized)
            | (Self::Planned, Self::Cancelled)
            | (Self::Planned, Self::Revoked) => true,

            // AUTHORIZED → QUEUED | CANCELLED | REVOKED
            (Self::Authorized, Self::Queued)
            | (Self::Authorized, Self::Cancelled)
            | (Self::Authorized, Self::Revoked) => true,

            // QUEUED → RUNNING | CANCELLED | FAILED
            (Self::Queued, Self::Running)
            | (Self::Queued, Self::Cancelled)
            | (Self::Queued, Self::Failed) => true,

            // RUNNING → SUCCEEDED | FAILED | UNKNOWN
            (Self::Running, Self::Succeeded)
            | (Self::Running, Self::Failed)
            | (Self::Running, Self::Unknown) => true,

            // UNKNOWN → RECONCILING | SUCCEEDED | FAILED
            (Self::Unknown, Self::Reconciling)
            | (Self::Unknown, Self::Succeeded)
            | (Self::Unknown, Self::Failed) => true,

            // RECONCILING → SUCCEEDED | FAILED | UNKNOWN
            (Self::Reconciling, Self::Succeeded)
            | (Self::Reconciling, Self::Failed)
            | (Self::Reconciling, Self::Unknown) => true,

            // SUCCEEDED → FAILED | UNKNOWN
            // (correction of premature success: actions_execution.rs marks
            // the action 'succeeded' when dispatching to the executor, before
            // the external intervention is confirmed. The community executor
            // may later correct this to 'failed' or 'unknown'.)
            (Self::Succeeded, Self::Failed) | (Self::Succeeded, Self::Unknown) => true,

            // Terminal states have no outgoing transitions.
            _ => false,
        }
    }
}

impl fmt::Display for ActionState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Error returned when an illegal state transition is attempted.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("illegal transition: {from} → {to}")]
pub struct IllegalTransition {
    pub from: ActionState,
    pub to: ActionState,
}

/// Attempts to transition from one state to another, returning an error
/// if the transition is illegal.
pub fn transition(from: ActionState, to: ActionState) -> Result<ActionState, IllegalTransition> {
    if from.can_transition_to(to) {
        Ok(to)
    } else {
        Err(IllegalTransition { from, to })
    }
}

// ── Canonical execution resolution ──────────────────────────────────

/// The exclusive delivery state reported by a provider adapter.
///
/// This replaces the former three-boolean `ProviderDelivery` struct
/// (`confirmed`, `definitive_failure`, `confirmation_lost`) which
/// allowed impossible combinations like `confirmed: true,
/// definitive_failure: true`. The enum makes those unrepresentable.
///
/// Provider adapters (`community_post_to_evidence`,
/// `outbox_event_to_evidence`) translate external state into this type.
/// The canonical resolver (`resolve_outcome`) consumes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderDeliveryState {
    /// Delivery confirmed — the external side effect happened.
    /// (e.g. `community_posts.status = 'posted'`, outbox `delivered`.)
    Confirmed,
    /// Definitive failure — the external side effect did NOT occur.
    /// (e.g. pre-Reddit rejection, no agents service, subreddit
    /// cooldown, permanent outbox rejection.)
    DefinitiveFailure,
    /// Confirmation lost — the intervention may have succeeded, but we
    /// cannot tell (e.g. worker crash during the Reddit API call). Only
    /// a human checking the external platform can resolve this.
    /// Maps to business-layer `UNKNOWN`.
    ConfirmationLost,
    /// In-flight — pending/posting/rate_limited/processing. No
    /// terminal evidence yet. Resolves later through the adapter's
    /// own paths.
    InFlight,
}

/// Observed facts about an execution — the pure domain input to the
/// canonical resolution matrix.
///
/// This type carries **no persistence details**: no SQL, no table names,
/// no worker internals. Provider-specific adapters convert their state
/// into these facts before calling [`resolve_outcome`].
///
/// # DDD boundary
///
/// `resolve_outcome()` must NOT know about SQL, Postgres,
/// `community_posts`, `autopilot_actions`, HTTP, or worker implementation
/// details. It operates only on these domain facts. Persistence code is
/// responsible for loading facts, calling `resolve_outcome()`, and
/// applying the resulting state transition atomically.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionEvidence {
    /// A terminal executor receipt was observed.
    TerminalReceipt {
        /// `true` = succeeded receipt, `false` = failed receipt.
        succeeded: bool,
    },
    /// A provider-specific delivery confirmation was observed.
    ///
    /// Used by `community.engage.request`: `community_posts` is the
    /// receipt, not an executor report. The adapter translates
    /// `community_posts.status` + `error_message` into
    /// [`ProviderDeliveryState`] and wraps it here.
    ProviderDelivery(ProviderDeliveryState),
    /// No new authoritative fact available. The receipt never arrived
    /// and no provider delivery state exists.
    ///
    /// CRITICAL: this maps to [`Resolution::NoChange`], never to
    /// `Failed` or `Executed`. "We don't know" and "we know it failed"
    /// are completely different.
    NoEvidence,
}

/// What the external world told us — the pure observation, before any
/// state-transition policy is applied.
///
/// This is the output of [`resolve_observation`]. It contains no
/// `NoChange` variant because "no change" is a transition-policy
/// decision, not an observation. An observation is always one of:
/// the intervention happened, it didn't, or we can't tell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservedResolution {
    /// The external intervention occurred.
    Executed,
    /// The external intervention definitively did NOT occur.
    Failed,
    /// Cannot establish whether the intervention occurred.
    Unknown,
}

/// The legal state transition resulting from an observation + current
/// state.
///
/// Output of [`legal_transition`]. This is where monotonicity and
/// state-machine policy live — NOT in the observation resolver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegalTransition {
    /// The transition is legal — apply the new state.
    Apply(ActionState),
    /// The transition is not legal or not needed — do not change state.
    /// This covers: already in the target state, monotonicity guards
    /// (late failure after prior success), and terminal-state protection.
    NoChange,
    /// A contradictory observation arrived for a terminal state.
    /// The observation contradicts the persisted state — this is
    /// information, not a silent coercion. The caller must surface
    /// this to operator visibility (log + ops watchdog), not silently
    /// pick the latest thing.
    ///
    /// Examples:
    /// - external fact = Confirmed, current state = Failed
    /// - external fact = DefinitiveFailure, current state = Succeeded
    Conflict,
}

/// The canonical resolution — what the execution outcome means for the
/// causal learner and the operational state machine.
///
/// This is the output of the compatibility wrapper [`resolve_outcome`].
/// New code should prefer calling [`resolve_observation`] +
/// [`legal_transition`] directly for clearer DDD separation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resolution {
    /// The external intervention occurred.
    /// Action → `succeeded`, assignment → `executed`.
    Executed,
    /// The external intervention definitively did NOT occur.
    /// Action → `failed`, assignment → `failed`.
    Failed,
    /// Cannot establish whether the intervention occurred.
    /// Action → `unknown`, assignment → `unknown`.
    /// NOT a failure, NOT a success — triggers reconciliation.
    Unknown,
    /// No new authoritative fact — do not change state.
    /// Returned for `NoEvidence` and for monotonicity guards
    /// (e.g. late failure after prior success).
    NoChange,
    /// A contradictory observation arrived for a terminal state.
    /// The observation contradicts the persisted state — this is
    /// information, not a silent coercion. The caller must surface
    /// this to operator visibility (log + ops watchdog), not silently
    /// pick the latest thing.
    Conflict,
}

/// Pure observation resolver: maps external facts to what happened.
///
/// This function knows NOTHING about the current action state, the
/// state machine, or monotonicity. It only answers: "given these
/// external facts, did the intervention happen, not happen, or can't
/// we tell?"
///
/// # Observation matrix
///
/// | Observed facts                              | ObservedResolution |
/// |---------------------------------------------|--------------------|
/// | `TerminalReceipt { succeeded: true }`       | `Executed`         |
/// | `TerminalReceipt { succeeded: false }`      | `Failed`           |
/// | `ProviderDelivery(Confirmed)`               | `Executed`         |
/// | `ProviderDelivery(DefinitiveFailure)`       | `Failed`           |
/// | `ProviderDelivery(ConfirmationLost)`        | `Unknown`          |
/// | `ProviderDelivery(InFlight)`                | `Unknown`          |
/// | `NoEvidence`                                | `Unknown`          |
///
/// # Critical invariant
///
/// `NoEvidence` → `Unknown`, NEVER `Failed`. "We don't know" and "we
/// know it failed" are completely different. The transition policy
/// ([`legal_transition`]) will prevent `Unknown` from overwriting a
/// terminal state.
#[must_use]
pub fn resolve_observation(evidence: ResolutionEvidence) -> ObservedResolution {
    match evidence {
        ResolutionEvidence::TerminalReceipt { succeeded: true } => ObservedResolution::Executed,
        ResolutionEvidence::TerminalReceipt { succeeded: false } => ObservedResolution::Failed,
        ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::Confirmed) => {
            ObservedResolution::Executed
        }
        ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::DefinitiveFailure) => {
            ObservedResolution::Failed
        }
        ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::ConfirmationLost) => {
            ObservedResolution::Unknown
        }
        ResolutionEvidence::ProviderDelivery(ProviderDeliveryState::InFlight) => {
            ObservedResolution::Unknown
        }
        ResolutionEvidence::NoEvidence => ObservedResolution::Unknown,
    }
}

/// State-transition policy: given the current action state and an
/// observation, determine the legal state transition.
///
/// This function encodes:
/// - **Monotonicity**: a late failure must not downgrade a prior success.
/// - **Terminal-state protection**: terminal states cannot be overwritten.
/// - **Idempotency**: if we're already in the target state, no change.
/// - **State machine legality**: transitions must follow
///   [`ActionState::can_transition_to`].
///
/// # Transition matrix
///
/// | Current state | Observation | Result |
/// |---------------|-------------|--------|
/// | `Succeeded` | `Executed` | `NoChange` (idempotent — already there) |
/// | `Succeeded` | `Failed` | `Conflict` (contradicts persisted success) |
/// | `Succeeded` | `Unknown` | `NoChange` (don't lose certainty) |
/// | `Failed` | `Executed` | `Conflict` (contradicts persisted failure) |
/// | `Failed` | `Failed` | `NoChange` (idempotent — already there) |
/// | `Failed` | `Unknown` | `NoChange` (terminal — no outgoing) |
/// | `Unknown` | `Executed` | `Apply(Succeeded)` |
/// | `Unknown` | `Failed` | `Apply(Failed)` |
/// | `Unknown` | `Unknown` | `NoChange` (no new info) |
/// | `Reconciling` | `Executed` | `Apply(Succeeded)` |
/// | `Reconciling` | `Failed` | `Apply(Failed)` |
/// | `Reconciling` | `Unknown` | `NoChange` (still unknown) |
/// | `Running` | `Executed` | `Apply(Succeeded)` |
/// | `Running` | `Failed` | `Apply(Failed)` |
/// | `Running` | `Unknown` | `Apply(Unknown)` |
///
/// # Conflict vs NoChange
///
/// `NoChange` means the observation is either idempotent (same direction
/// as the current state) or non-informative (Unknown when already
/// terminal). The state is correct; no action needed.
///
/// `Conflict` means the observation **contradicts** the persisted state.
/// This is information — the caller must surface it to operator
/// visibility, not silently coerce it. Examples:
/// - external fact = Confirmed, current state = Failed
/// - external fact = DefinitiveFailure, current state = Succeeded
#[must_use]
pub fn legal_transition(current: ActionState, observed: ObservedResolution) -> LegalTransition {
    match (current, observed) {
        // ── Succeeded: idempotent for Executed, Conflict for Failed ──
        // A second success is a no-op. A late failure contradicts the
        // persisted success — surface as Conflict, don't silently downgrade.
        // A late unknown must not lose certainty.
        (ActionState::Succeeded, ObservedResolution::Executed) => LegalTransition::NoChange,
        (ActionState::Succeeded, ObservedResolution::Failed) => LegalTransition::Conflict,
        (ActionState::Succeeded, ObservedResolution::Unknown) => LegalTransition::NoChange,

        // ── Failed: idempotent for Failed, Conflict for Executed ──
        // A late success contradicts the persisted failure — surface as
        // Conflict, don't silently revive. A late unknown is non-informative.
        (ActionState::Failed, ObservedResolution::Executed) => LegalTransition::Conflict,
        (ActionState::Failed, ObservedResolution::Failed) => LegalTransition::NoChange,
        (ActionState::Failed, ObservedResolution::Unknown) => LegalTransition::NoChange,

        // ── Cancelled / Revoked: terminal — no outgoing transitions ──
        (ActionState::Cancelled, _) => LegalTransition::NoChange,
        (ActionState::Revoked, _) => LegalTransition::NoChange,

        // ── Unknown: can resolve to Succeeded or Failed ──
        (ActionState::Unknown, ObservedResolution::Executed) => {
            LegalTransition::Apply(ActionState::Succeeded)
        }
        (ActionState::Unknown, ObservedResolution::Failed) => {
            LegalTransition::Apply(ActionState::Failed)
        }
        (ActionState::Unknown, ObservedResolution::Unknown) => LegalTransition::NoChange,

        // ── Reconciling: can resolve to Succeeded or Failed ──
        (ActionState::Reconciling, ObservedResolution::Executed) => {
            LegalTransition::Apply(ActionState::Succeeded)
        }
        (ActionState::Reconciling, ObservedResolution::Failed) => {
            LegalTransition::Apply(ActionState::Failed)
        }
        (ActionState::Reconciling, ObservedResolution::Unknown) => LegalTransition::NoChange,

        // ── Running: can transition to Succeeded, Failed, or Unknown ──
        (ActionState::Running, ObservedResolution::Executed) => {
            LegalTransition::Apply(ActionState::Succeeded)
        }
        (ActionState::Running, ObservedResolution::Failed) => {
            LegalTransition::Apply(ActionState::Failed)
        }
        (ActionState::Running, ObservedResolution::Unknown) => {
            LegalTransition::Apply(ActionState::Unknown)
        }

        // ── Pre-execution states: observations don't apply ──
        // Planned, Authorized, Queued haven't been dispatched yet.
        // An observation about execution is not meaningful here.
        (ActionState::Planned, _) | (ActionState::Authorized, _) | (ActionState::Queued, _) => {
            LegalTransition::NoChange
        }
    }
}

/// Compatibility wrapper: combines [`resolve_observation`] and
/// [`legal_transition`] into a single call.
///
/// Callers that need the current action state for transition policy
/// should pass it here. The function:
/// 1. Resolves the evidence to a pure observation.
/// 2. Applies the legal-transition policy given the current state.
/// 3. Maps the result to a [`Resolution`].
///
/// ALL reconciliation implementations MUST use this function (or call
/// [`resolve_observation`] + [`legal_transition`] directly):
/// - `record_execution_report` (normal receipt path)
/// - `receipt_reconciliation.rs` (gap/late/worker path)
/// - `viryaos_action_ledger_reconcile()` SQL function (manual fallback)
///
/// Do NOT duplicate this semantic matrix in callers.
///
/// # Critical invariants
///
/// - `NoEvidence` → observation is `Unknown` → `NoChange` from terminal
///   states. A missing fact must NEVER default to `Failed` and must NOT
///   silently become `Executed`.
/// - `Unknown` means execution may have happened but cannot be established.
///   It is NOT a failure and NOT a success.
/// - A late failure after a prior success → `Conflict` (contradiction):
///   `legal_transition(Succeeded, Failed) → Conflict`. The caller must
///   surface this to operator visibility, not silently downgrade.
/// - A late success after a prior failure → `Conflict` (contradiction):
///   `legal_transition(Failed, Executed) → Conflict`. The caller must
///   surface this, not silently revive.
#[must_use]
pub fn resolve_outcome(evidence: ResolutionEvidence, current_state: ActionState) -> Resolution {
    let observed = resolve_observation(evidence);
    match legal_transition(current_state, observed) {
        LegalTransition::Apply(ActionState::Succeeded) => Resolution::Executed,
        LegalTransition::Apply(ActionState::Failed) => Resolution::Failed,
        LegalTransition::Apply(ActionState::Unknown) => Resolution::Unknown,
        LegalTransition::Apply(_) => Resolution::NoChange,
        LegalTransition::NoChange => Resolution::NoChange,
        LegalTransition::Conflict => Resolution::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_have_no_outgoing_transitions() {
        // SUCCEEDED is NOT terminal — it can be corrected to FAILED or UNKNOWN
        // by the community executor when a premature success marking needs
        // correction. Only Failed, Cancelled, and Revoked are truly terminal.
        for state in [
            ActionState::Failed,
            ActionState::Cancelled,
            ActionState::Revoked,
        ] {
            for target in [
                ActionState::Planned,
                ActionState::Authorized,
                ActionState::Queued,
                ActionState::Running,
                ActionState::Succeeded,
                ActionState::Failed,
                ActionState::Unknown,
                ActionState::Reconciling,
                ActionState::Cancelled,
                ActionState::Revoked,
            ] {
                assert!(
                    !state.can_transition_to(target),
                    "{state:?} should not transition to {target:?}"
                );
            }
        }
    }

    #[test]
    fn succeeded_can_correct_to_failed_or_unknown() {
        // SUCCEEDED is not terminal in the community executor path —
        // the executor may correct a premature success to FAILED or UNKNOWN.
        assert!(ActionState::Succeeded.can_transition_to(ActionState::Failed));
        assert!(ActionState::Succeeded.can_transition_to(ActionState::Unknown));
        // But not to other states.
        assert!(!ActionState::Succeeded.can_transition_to(ActionState::Running));
        assert!(!ActionState::Succeeded.can_transition_to(ActionState::Reconciling));
        assert!(!ActionState::Succeeded.can_transition_to(ActionState::Cancelled));
    }

    #[test]
    fn forward_transitions_are_allowed() {
        assert!(ActionState::Planned.can_transition_to(ActionState::Authorized));
        assert!(ActionState::Authorized.can_transition_to(ActionState::Queued));
        assert!(ActionState::Queued.can_transition_to(ActionState::Running));
        assert!(ActionState::Running.can_transition_to(ActionState::Succeeded));
        assert!(ActionState::Running.can_transition_to(ActionState::Failed));
        assert!(ActionState::Running.can_transition_to(ActionState::Unknown));
    }

    #[test]
    fn backward_transitions_are_rejected() {
        assert!(!ActionState::Running.can_transition_to(ActionState::Queued));
        assert!(!ActionState::Queued.can_transition_to(ActionState::Authorized));
        assert!(!ActionState::Authorized.can_transition_to(ActionState::Planned));
        assert!(!ActionState::Succeeded.can_transition_to(ActionState::Running));
    }

    #[test]
    fn unknown_can_reconcile() {
        assert!(ActionState::Unknown.can_transition_to(ActionState::Reconciling));
        assert!(ActionState::Reconciling.can_transition_to(ActionState::Succeeded));
        assert!(ActionState::Reconciling.can_transition_to(ActionState::Failed));
        // Reconciliation may not resolve — back to UNKNOWN for another attempt
        assert!(ActionState::Reconciling.can_transition_to(ActionState::Unknown));
    }

    #[test]
    fn unknown_is_not_terminal() {
        assert!(!ActionState::Unknown.is_terminal());
        assert!(ActionState::Unknown.is_active());
    }

    #[test]
    fn transition_returns_target_on_success() {
        let result = transition(ActionState::Planned, ActionState::Authorized);
        assert_eq!(result, Ok(ActionState::Authorized));
    }

    #[test]
    fn transition_returns_error_on_illegal() {
        let result = transition(ActionState::Succeeded, ActionState::Running);
        assert!(result.is_err());
    }

    #[test]
    fn state_round_trips_through_string() {
        for state in [
            ActionState::Planned,
            ActionState::Authorized,
            ActionState::Queued,
            ActionState::Running,
            ActionState::Succeeded,
            ActionState::Failed,
            ActionState::Unknown,
            ActionState::Reconciling,
            ActionState::Cancelled,
            ActionState::Revoked,
        ] {
            assert_eq!(ActionState::parse(state.as_str()), Some(state));
        }
        assert_eq!(ActionState::parse("invalid"), None);
    }

    // ── resolve_observation pure mapping tests ──

    #[test]
    fn observation_terminal_receipt_success_is_executed() {
        assert_eq!(
            resolve_observation(ResolutionEvidence::TerminalReceipt { succeeded: true }),
            ObservedResolution::Executed
        );
    }

    #[test]
    fn observation_terminal_receipt_failure_is_failed() {
        assert_eq!(
            resolve_observation(ResolutionEvidence::TerminalReceipt { succeeded: false }),
            ObservedResolution::Failed
        );
    }

    #[test]
    fn observation_provider_delivery_confirmed_is_executed() {
        assert_eq!(
            resolve_observation(ResolutionEvidence::ProviderDelivery(
                ProviderDeliveryState::Confirmed
            )),
            ObservedResolution::Executed
        );
    }

    #[test]
    fn observation_provider_delivery_definitive_failure_is_failed() {
        assert_eq!(
            resolve_observation(ResolutionEvidence::ProviderDelivery(
                ProviderDeliveryState::DefinitiveFailure
            )),
            ObservedResolution::Failed
        );
    }

    #[test]
    fn observation_provider_delivery_confirmation_lost_is_unknown() {
        assert_eq!(
            resolve_observation(ResolutionEvidence::ProviderDelivery(
                ProviderDeliveryState::ConfirmationLost
            )),
            ObservedResolution::Unknown
        );
    }

    #[test]
    fn observation_provider_delivery_in_flight_is_unknown() {
        assert_eq!(
            resolve_observation(ResolutionEvidence::ProviderDelivery(
                ProviderDeliveryState::InFlight
            )),
            ObservedResolution::Unknown
        );
    }

    #[test]
    fn observation_no_evidence_is_unknown() {
        // CRITICAL: NoEvidence → Unknown, NEVER Failed.
        // "We don't know" and "we know it failed" are completely different.
        assert_eq!(
            resolve_observation(ResolutionEvidence::NoEvidence),
            ObservedResolution::Unknown
        );
    }

    // ── legal_transition state-policy tests ──

    #[test]
    fn legal_unknown_plus_executed_applies_succeeded() {
        assert_eq!(
            legal_transition(ActionState::Unknown, ObservedResolution::Executed),
            LegalTransition::Apply(ActionState::Succeeded)
        );
    }

    #[test]
    fn legal_unknown_plus_failed_applies_failed() {
        assert_eq!(
            legal_transition(ActionState::Unknown, ObservedResolution::Failed),
            LegalTransition::Apply(ActionState::Failed)
        );
    }

    #[test]
    fn legal_unknown_plus_unknown_is_no_change() {
        assert_eq!(
            legal_transition(ActionState::Unknown, ObservedResolution::Unknown),
            LegalTransition::NoChange
        );
    }

    #[test]
    fn legal_succeeded_plus_failed_is_conflict() {
        // A late failure contradicts a persisted success. This is NOT
        // silently downgraded — it's surfaced as Conflict for operator
        // visibility. The state machine does not pick the latest thing.
        assert_eq!(
            legal_transition(ActionState::Succeeded, ObservedResolution::Failed),
            LegalTransition::Conflict
        );
    }

    #[test]
    fn legal_succeeded_plus_executed_is_no_change_idempotent() {
        assert_eq!(
            legal_transition(ActionState::Succeeded, ObservedResolution::Executed),
            LegalTransition::NoChange
        );
    }

    #[test]
    fn legal_succeeded_plus_unknown_is_no_change_no_certainty_loss() {
        // Don't lose certainty from a late ambiguous signal.
        assert_eq!(
            legal_transition(ActionState::Succeeded, ObservedResolution::Unknown),
            LegalTransition::NoChange
        );
    }

    #[test]
    fn legal_failed_plus_executed_is_conflict_cannot_revive() {
        // A late success contradicts a persisted failure. This is NOT
        // silently revived — it's surfaced as Conflict for operator
        // visibility.
        assert_eq!(
            legal_transition(ActionState::Failed, ObservedResolution::Executed),
            LegalTransition::Conflict
        );
    }

    #[test]
    fn legal_failed_plus_failed_is_no_change_idempotent() {
        assert_eq!(
            legal_transition(ActionState::Failed, ObservedResolution::Failed),
            LegalTransition::NoChange
        );
    }

    #[test]
    fn legal_failed_plus_unknown_is_no_change_terminal() {
        assert_eq!(
            legal_transition(ActionState::Failed, ObservedResolution::Unknown),
            LegalTransition::NoChange
        );
    }

    #[test]
    fn legal_reconciling_plus_executed_applies_succeeded() {
        assert_eq!(
            legal_transition(ActionState::Reconciling, ObservedResolution::Executed),
            LegalTransition::Apply(ActionState::Succeeded)
        );
    }

    #[test]
    fn legal_reconciling_plus_failed_applies_failed() {
        assert_eq!(
            legal_transition(ActionState::Reconciling, ObservedResolution::Failed),
            LegalTransition::Apply(ActionState::Failed)
        );
    }

    #[test]
    fn legal_running_plus_unknown_applies_unknown() {
        assert_eq!(
            legal_transition(ActionState::Running, ObservedResolution::Unknown),
            LegalTransition::Apply(ActionState::Unknown)
        );
    }

    #[test]
    fn legal_pre_execution_states_are_no_change() {
        // Observations about execution are not meaningful before dispatch.
        for state in [
            ActionState::Planned,
            ActionState::Authorized,
            ActionState::Queued,
        ] {
            assert_eq!(
                legal_transition(state, ObservedResolution::Executed),
                LegalTransition::NoChange,
                "{state:?} + Executed should be NoChange"
            );
        }
    }

    // ── resolve_outcome compatibility wrapper tests ──

    #[test]
    fn resolve_outcome_unknown_plus_success_is_executed() {
        assert_eq!(
            resolve_outcome(
                ResolutionEvidence::TerminalReceipt { succeeded: true },
                ActionState::Unknown
            ),
            Resolution::Executed
        );
    }

    #[test]
    fn resolve_outcome_unknown_plus_failure_is_failed() {
        assert_eq!(
            resolve_outcome(
                ResolutionEvidence::TerminalReceipt { succeeded: false },
                ActionState::Unknown
            ),
            Resolution::Failed
        );
    }

    #[test]
    fn resolve_outcome_succeeded_plus_failure_is_conflict() {
        // A late failure contradicts a persisted success → Conflict.
        // The caller must surface this, not silently downgrade.
        assert_eq!(
            resolve_outcome(
                ResolutionEvidence::TerminalReceipt { succeeded: false },
                ActionState::Succeeded
            ),
            Resolution::Conflict
        );
    }

    #[test]
    fn resolve_outcome_no_evidence_from_unknown_is_no_change() {
        assert_eq!(
            resolve_outcome(ResolutionEvidence::NoEvidence, ActionState::Unknown),
            Resolution::NoChange
        );
    }

    #[test]
    fn resolve_outcome_no_evidence_never_becomes_failed() {
        // The most important invariant: a missing fact must not default
        // to failure. If this breaks, the causal learner will silently
        // treat missing receipts as failures, corrupting the treatment-
        // effect posterior.
        assert_ne!(
            resolve_outcome(ResolutionEvidence::NoEvidence, ActionState::Unknown),
            Resolution::Failed
        );
        assert_ne!(
            resolve_outcome(ResolutionEvidence::NoEvidence, ActionState::Unknown),
            Resolution::Executed
        );
    }
}
