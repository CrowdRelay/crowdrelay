-- Evidence events — truly immutable append-only event log.
--
-- The existing `viryaos_growth_evidence` table is named "immutable" but is
-- actually mutable: dispatch creates a row, measurement updates it, and
-- conversion updates it again. This makes event ordering and corrections
-- hard to audit.
--
-- This table is a true event-sourced log: each row is an immutable fact that
-- happened at a specific time. The derived `viryaos_growth_episodes` table
-- (migration 0155) is the aggregate state rebuilt from these events.
--
-- Design:
-- - INSERT only — no UPDATE or DELETE (enforced by application convention).
-- - `event_type` classifies the event (dispatch, reach, exposure, response,
--   conversion, durability measurement, etc.).
-- - `payload` is a type-specific JSON blob with the event details.
-- - `seq` is a monotonically increasing sequence for ordering within a
--   workspace.
-- - `occurred_at` is the immutable timestamp of the event.

CREATE TABLE IF NOT EXISTS viryaos_evidence_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- The action this event relates to (optional — some events may not have
    -- an action, e.g. organic fan growth).
    action_id uuid REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,

    -- The opportunity this event relates to (optional).
    opportunity_id text,

    -- The episode this event belongs to (links to the episode aggregate).
    episode_id text,

    -- The event type: what happened.
    event_type text NOT NULL CHECK (event_type IN (
        'action_dispatched',       -- an autopilot action was dispatched
        'reach_attempted',          -- a reach event was recorded
        'exposure_recorded',        -- audience exposure was recorded
        'response_received',        -- a response was received (reply, click)
        'conversion_observed',      -- a fan conversion was observed
        'fan_still_active_day_30',  -- 30-day durability check: fan active
        'fan_churned_day_30',       -- 30-day durability check: fan churned
        'measurement_resolved',     -- the measurement window closed
        'treatment_assigned'        -- a treatment was assigned (A/B test)
    )),

    -- The event payload (type-specific JSON).
    payload jsonb NOT NULL DEFAULT '{}'::jsonb,

    -- When the event occurred (immutable).
    occurred_at timestamptz NOT NULL DEFAULT now(),

    -- Monotonically increasing sequence for ordering within a workspace.
    seq bigserial NOT NULL,

    created_at timestamptz NOT NULL DEFAULT now()
);

-- Indexes for the brain's most common queries.
CREATE INDEX IF NOT EXISTS idx_evidence_events_workspace_seq
    ON viryaos_evidence_events (workspace_id, seq);

CREATE INDEX IF NOT EXISTS idx_evidence_events_workspace_occurred
    ON viryaos_evidence_events (workspace_id, occurred_at DESC);

CREATE INDEX IF NOT EXISTS idx_evidence_events_action
    ON viryaos_evidence_events (action_id)
    WHERE action_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_evidence_events_episode
    ON viryaos_evidence_events (workspace_id, episode_id)
    WHERE episode_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_evidence_events_type
    ON viryaos_evidence_events (workspace_id, event_type);
