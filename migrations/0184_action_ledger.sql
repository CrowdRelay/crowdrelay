-- Action Ledger: the canonical execution state for every autopilot action.
--
-- The ledger is an append-only, history-safe table. The current row is a
-- projection — the full transition history is auditable through the trace
-- timeline (which joins outbox_events, audit_events, evidence_events, etc.).
--
-- State transitions (monotonic — backwards transitions are rejected):
--   PLANNED     → AUTHORIZED | CANCELLED | REVOKED
--   AUTHORIZED  → QUEUED | CANCELLED | REVOKED
--   QUEUED      → RUNNING | CANCELLED | FAILED
--   RUNNING     → SUCCEEDED | FAILED | UNKNOWN
--   UNKNOWN     → RECONCILING | SUCCEEDED | FAILED
--   RECONCILING → SUCCEEDED | FAILED | UNKNOWN
--   SUCCEEDED   → (terminal)
--   FAILED      → (terminal)
--   CANCELLED   → (terminal)
--   REVOKED     → (terminal)
--
-- UNKNOWN semantics:
--   "CrowdRelay cannot establish whether the external side effect happened."
--   UNKNOWN is NOT a failure — retry mechanisms must NOT treat it as one
--   (to avoid duplicate side effects). Instead, UNKNOWN triggers
--   reconciliation, which may resolve to SUCCEEDED or FAILED.
--
-- The ledger is populated by triggers on viryaos_autopilot_actions status
-- changes, ensuring the ledger always reflects the current action state.

CREATE TABLE IF NOT EXISTS viryaos_action_ledger (
    -- The action this ledger entry tracks (1:1 with autopilot_actions)
    action_id uuid PRIMARY KEY REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,

    -- The current state of the action (see transition rules above)
    state text NOT NULL DEFAULT 'PLANNED'
        CONSTRAINT viryaos_action_ledger_state_valid
        CHECK (state IN ('PLANNED', 'AUTHORIZED', 'QUEUED', 'RUNNING',
                         'SUCCEEDED', 'FAILED', 'UNKNOWN', 'RECONCILING',
                         'CANCELLED', 'REVOKED')),

    -- The trace_id for this action's lifecycle (nullable for legacy rows)
    trace_id uuid,

    -- The decision that created this action (nullable for non-autopilot actions)
    decision_id uuid,

    -- When the action entered the current state
    state_entered_at timestamptz NOT NULL DEFAULT now(),

    -- When the ledger entry was last updated
    updated_at timestamptz NOT NULL DEFAULT now(),

    -- How many times the state has changed (for retry tracking)
    transition_count integer NOT NULL DEFAULT 0,

    -- The previous state (for audit — the full history is in the trace timeline)
    previous_state text
        CONSTRAINT viryaos_action_ledger_previous_state_valid
        CHECK (previous_state IS NULL OR previous_state IN (
            'PLANNED', 'AUTHORIZED', 'QUEUED', 'RUNNING',
            'SUCCEEDED', 'FAILED', 'UNKNOWN', 'RECONCILING',
            'CANCELLED', 'REVOKED'
        )),

    -- Reconciliation count: how many times we've tried to resolve UNKNOWN
    reconciliation_count integer NOT NULL DEFAULT 0,

    -- Last reconciliation error (if any)
    last_reconciliation_error text,

    -- UNIQUE (workspace_id, action_id) — already implied by PK, but explicit
    -- for clarity and to match the pattern used by other tables.
    UNIQUE (workspace_id, action_id)
);

-- Index for workspace-scoped queries (list all actions in a workspace)
CREATE INDEX IF NOT EXISTS action_ledger_workspace_idx
    ON viryaos_action_ledger (workspace_id, state_entered_at DESC);

-- Index for trace_id lookups (find all actions in a trace)
CREATE INDEX IF NOT EXISTS action_ledger_trace_idx
    ON viryaos_action_ledger (workspace_id, trace_id) WHERE trace_id IS NOT NULL;

-- Index for UNKNOWN state queries (reconciliation sweep)
CREATE INDEX IF NOT EXISTS action_ledger_unknown_idx
    ON viryaos_action_ledger (workspace_id, state_entered_at)
    WHERE state = 'UNKNOWN';

-- Index for non-terminal states (active action queries)
CREATE INDEX IF NOT EXISTS action_ledger_active_idx
    ON viryaos_action_ledger (workspace_id, state)
    WHERE state IN ('PLANNED', 'AUTHORIZED', 'QUEUED', 'RUNNING', 'UNKNOWN', 'RECONCILING');

-- Append-only protection: the ledger row may be UPDATEd (state transition)
-- but never DELETEd directly (only via CASCADE from the action).
CREATE TRIGGER action_ledger_append_only
BEFORE DELETE ON viryaos_action_ledger
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_append_only_mutation();

-- Backfill from existing autopilot actions.
-- Map the existing action status to the ledger state.
INSERT INTO viryaos_action_ledger (action_id, workspace_id, state, trace_id, decision_id, state_entered_at, transition_count)
SELECT
    a.id,
    a.workspace_id,
    CASE a.status
        WHEN 'awaiting_approval' THEN 'AUTHORIZED'
        WHEN 'queued' THEN 'QUEUED'
        WHEN 'in_progress' THEN 'RUNNING'
        WHEN 'succeeded' THEN 'SUCCEEDED'
        WHEN 'failed' THEN 'FAILED'
        WHEN 'cancelled' THEN 'CANCELLED'
        WHEN 'parked' THEN 'PLANNED'
        ELSE 'PLANNED'
    END,
    a.trace_id,
    a.decision_id,
    COALESCE(a.updated_at, a.created_at, now()),
    0
FROM viryaos_autopilot_actions a
WHERE NOT EXISTS (
    SELECT 1 FROM viryaos_action_ledger l WHERE l.action_id = a.id
)
ON CONFLICT (action_id) DO NOTHING;
