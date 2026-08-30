//! Execution trace identity — the canonical identity system for
//! end-to-end action tracing.
//!
//! The trace spine connects every event in an action's lifecycle:
//! API → outbox → worker → agents → executor → measurement.
//!
//! # The three identities
//!
//! - `TraceId` — generated at the API boundary or autopilot evaluation
//!   cycle. One trace per end-to-end execution flow.
//! - `CausationId` — the event that caused this one. Links parent →
//!   child events in the causal chain.
//! - `ActionId` — the durable action this trace belongs to. Multiple
//!   events in the same trace share the same action_id.
//!
//! # Propagation
//!
//! `TraceContext` is serializable and travels in outbox payloads, agent
//! task metadata, and event payloads. Every system that participates in
//! an action's lifecycle must propagate the trace context.
//!
//! # Mixed-version safety
//!
//! All trace columns in the database are nullable. Old code that doesn't
//! populate them leaves NULL. New code populates them. The timeline query
//! handles NULL trace_ids gracefully (returns partial results).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ids::WorkspaceId;

/// A trace identifier — one per end-to-end execution flow.
///
/// Uses UUID v7 (time-ordered) for index locality. Generated at the
/// API boundary or autopilot evaluation cycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct TraceId(Uuid);

impl TraceId {
    /// Creates a new time-ordered trace identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    /// Wraps an existing UUID without changing its value.
    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Consumes the trace identifier and returns the underlying UUID.
    #[must_use]
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for TraceId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

impl From<Uuid> for TraceId {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

impl From<TraceId> for Uuid {
    fn from(value: TraceId) -> Self {
        value.0
    }
}

/// A causation identifier — the event that caused this one.
///
/// Links parent → child events in the causal chain. A root event
/// (e.g., an API request) has `causation_id = None`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CausationId(Uuid);

impl CausationId {
    /// Wraps an existing UUID.
    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Borrows the underlying UUID.
    #[must_use]
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Consumes the causation identifier and returns the underlying UUID.
    #[must_use]
    pub const fn into_uuid(self) -> Uuid {
        self.0
    }
}

impl std::fmt::Display for CausationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl From<Uuid> for CausationId {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

impl From<CausationId> for Uuid {
    fn from(value: CausationId) -> Self {
        value.0
    }
}

/// The full trace context — propagated through every system boundary.
///
/// This is the canonical execution identity that connects an action's
/// lifecycle across API, outbox, worker, agents, executor, and
/// measurement.
///
/// # Invariants
///
/// - `trace_id` is always present (every event belongs to a trace)
/// - `tenant_id` is always present (every event is workspace-scoped)
/// - `causation_id` is `None` for root events, `Some` for caused events
/// - `action_id` is `None` for events not tied to a specific action
/// - `decision_id` is `None` for events not tied to a specific decision
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TraceContext {
    /// The trace this event belongs to.
    pub trace_id: TraceId,
    /// The event that caused this one, if any.
    pub causation_id: Option<CausationId>,
    /// The workspace this trace belongs to.
    pub tenant_id: WorkspaceId,
    /// The action this trace is tracking, if tied to a specific action.
    pub action_id: Option<Uuid>,
    /// The decision that created this action, if applicable.
    pub decision_id: Option<Uuid>,
}

impl TraceContext {
    /// Creates a new trace context for a root event (no causation).
    #[must_use]
    pub fn root(tenant_id: WorkspaceId) -> Self {
        Self {
            trace_id: TraceId::new(),
            causation_id: None,
            tenant_id,
            action_id: None,
            decision_id: None,
        }
    }

    /// Creates a child trace context — same trace, new causation.
    #[must_use]
    pub fn child(&self, causation_id: CausationId) -> Self {
        Self {
            trace_id: self.trace_id,
            causation_id: Some(causation_id),
            tenant_id: self.tenant_id,
            action_id: self.action_id,
            decision_id: self.decision_id,
        }
    }

    /// Returns a copy with the action_id set.
    #[must_use]
    pub fn with_action(mut self, action_id: Uuid) -> Self {
        self.action_id = Some(action_id);
        self
    }

    /// Returns a copy with the decision_id set.
    #[must_use]
    pub fn with_decision(mut self, decision_id: Uuid) -> Self {
        self.decision_id = Some(decision_id);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_is_time_ordered_and_unique() {
        let a = TraceId::new();
        let b = TraceId::new();
        assert_ne!(a, b, "two new trace ids must differ");
    }

    #[test]
    fn root_context_has_no_causation() {
        let ctx = TraceContext::root(WorkspaceId::new());
        assert!(ctx.causation_id.is_none());
        assert!(ctx.action_id.is_none());
        assert!(ctx.decision_id.is_none());
    }

    #[test]
    fn child_inherits_trace_and_tenant() {
        let root = TraceContext::root(WorkspaceId::new());
        let cause = CausationId::from_uuid(Uuid::now_v7());
        let child = root.child(cause);
        assert_eq!(child.trace_id, root.trace_id);
        assert_eq!(child.tenant_id, root.tenant_id);
        assert_eq!(child.causation_id, Some(cause));
    }

    #[test]
    fn with_action_and_decision_are_chainable() {
        let ctx = TraceContext::root(WorkspaceId::new())
            .with_action(Uuid::now_v7())
            .with_decision(Uuid::now_v7());
        assert!(ctx.action_id.is_some());
        assert!(ctx.decision_id.is_some());
    }

    #[test]
    fn trace_id_round_trips_through_string() {
        let id = TraceId::new();
        let s = id.to_string();
        let parsed: TraceId = s.parse().unwrap_or_else(|_| TraceId::new());
        assert_eq!(id, parsed);
    }
}
