-- Trace Spine: add trace context columns to existing tables.
--
-- These columns establish the canonical execution identity system:
--   trace_id     — one per end-to-end execution flow (UUID v7)
--   causation_id — the event that caused this one (links parent → child)
--   action_id    — the durable action this trace belongs to
--
-- All columns are nullable — existing rows have NULL trace_id. New writes
-- populate them. This is safe for mixed-version deployments: old code
-- ignores the columns, new code populates them.
--
-- The timeline query joins these tables by trace_id to reconstruct the
-- full causal chain of an action's lifecycle.

-- outbox_events: already has request_id; add trace context
ALTER TABLE outbox_events
    ADD COLUMN IF NOT EXISTS trace_id uuid,
    ADD COLUMN IF NOT EXISTS causation_id uuid,
    ADD COLUMN IF NOT EXISTS action_id uuid;

-- viryaos_autopilot_actions: add trace context
ALTER TABLE viryaos_autopilot_actions
    ADD COLUMN IF NOT EXISTS trace_id uuid,
    ADD COLUMN IF NOT EXISTS causation_id uuid;

-- viryaos_autopilot_decisions: add trace_id (decisions start a trace)
ALTER TABLE viryaos_autopilot_decisions
    ADD COLUMN IF NOT EXISTS trace_id uuid;

-- viryaos_autopilot_action_attempts: add trace_id
ALTER TABLE viryaos_autopilot_action_attempts
    ADD COLUMN IF NOT EXISTS trace_id uuid;

-- viryaos_autopilot_measurements: add trace_id
ALTER TABLE viryaos_autopilot_measurements
    ADD COLUMN IF NOT EXISTS trace_id uuid;

-- agent_outcomes: add trace_id (for cross-service trace)
ALTER TABLE agent_outcomes
    ADD COLUMN IF NOT EXISTS trace_id uuid;

-- audit_events: add trace_id (already has request_id)
ALTER TABLE audit_events
    ADD COLUMN IF NOT EXISTS trace_id uuid,
    ADD COLUMN IF NOT EXISTS action_id uuid;

-- viryaos_evidence_events: add trace_id
ALTER TABLE viryaos_evidence_events
    ADD COLUMN IF NOT EXISTS trace_id uuid;

-- Indexes for trace lookups (all partial — only where trace_id IS NOT NULL).
-- These keep the index small and fast for the common case (old rows have
-- NULL trace_id and are excluded from the index).
CREATE INDEX IF NOT EXISTS outbox_events_trace_idx
    ON outbox_events (workspace_id, trace_id) WHERE trace_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS autopilot_actions_trace_idx
    ON viryaos_autopilot_actions (workspace_id, trace_id) WHERE trace_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS autopilot_decisions_trace_idx
    ON viryaos_autopilot_decisions (workspace_id, trace_id) WHERE trace_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS autopilot_action_attempts_trace_idx
    ON viryaos_autopilot_action_attempts (workspace_id, trace_id) WHERE trace_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS autopilot_measurements_trace_idx
    ON viryaos_autopilot_measurements (workspace_id, trace_id) WHERE trace_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS agent_outcomes_trace_idx
    ON agent_outcomes (workspace_id, trace_id) WHERE trace_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS audit_events_trace_idx
    ON audit_events (workspace_id, trace_id) WHERE trace_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS evidence_events_trace_idx
    ON viryaos_evidence_events (workspace_id, trace_id) WHERE trace_id IS NOT NULL;
