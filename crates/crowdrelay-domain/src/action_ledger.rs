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
}
