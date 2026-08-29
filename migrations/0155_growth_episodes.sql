-- Growth episodes — derived aggregate from evidence events.
--
-- This table is the derived view of a growth episode: the brain's current
-- best understanding of what happened for a given autopilot action. It's
-- rebuilt from the immutable `viryaos_evidence_events` table.
--
-- Unlike the events table, this table IS mutable: it's updated whenever a
-- new event arrives for the action. But every update can be traced back to
-- a specific event in the log.
--
-- This table replaces `viryaos_growth_evidence` (migration 0150) as the
-- brain's primary read path for evidence. The old table is kept for
-- backward compatibility during the transition period.

CREATE TABLE IF NOT EXISTS viryaos_growth_episodes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- The autopilot action this episode tracks.
    action_id uuid NOT NULL REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,

    -- The opportunity and episode identifiers.
    opportunity_id text,
    episode_id text,

    -- Dispatch-time fields (from the 'action_dispatched' event).
    channel text,
    estimated_reach integer,
    treatment text,
    propensity double precision,
    predicted_fans double precision,
    predicted_signal_installs double precision,
    context jsonb,

    -- Outcome fields (filled from later events).
    observed_fans double precision,
    observed_incremental_fans double precision,
    durable_fans_30d double precision,
    actual_reach integer,
    converted boolean,

    -- When the episode was resolved (measurement window closed).
    resolved_at timestamptz,

    -- Updated whenever a new event arrives for this action.
    updated_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),

    -- One episode per action.
    UNIQUE (workspace_id, action_id)
);

-- Indexes for the brain's most common queries.
CREATE INDEX IF NOT EXISTS idx_growth_episodes_workspace_updated
    ON viryaos_growth_episodes (workspace_id, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_growth_episodes_opportunity
    ON viryaos_growth_episodes (workspace_id, opportunity_id)
    WHERE opportunity_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_growth_episodes_resolved
    ON viryaos_growth_episodes (workspace_id, resolved_at)
    WHERE resolved_at IS NOT NULL;
