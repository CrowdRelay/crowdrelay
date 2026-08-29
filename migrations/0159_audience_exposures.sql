-- Audience exposures — identity-level exposure tracking.
--
-- The `LearnedOverlapModel` infers audience overlap from residual performance
-- (expected_sum - observed_combined). This is confounded: a failed pair
-- could mean overlap, or both actions were poor, or timing was terrible.
--
-- This table records identity-level exposure: when a fan is exposed to
-- multiple actions (e.g. saw a Reddit post AND received a Signal push),
-- we record each exposure. Overlap is then computed from actual shared
-- audience identity, not from residual performance.
--
-- Design:
-- - `fan_id` is set when the exposed person is a known fan.
-- - `anonymous_id` is a pseudo-identity for non-fan exposures (e.g. Reddit
--   username hash). Either `fan_id` or `anonymous_id` must be set.
-- - `reach_event_id` links back to the reach event that produced the exposure.
-- - `audience_key` is a normalized key for the audience (e.g. "r_MetalMusic").

CREATE TABLE IF NOT EXISTS viryaos_audience_exposures (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- The fan who was exposed (optional — may be anonymous).
    fan_id uuid REFERENCES fans(id) ON DELETE SET NULL,

    -- A pseudo-identity for non-fan exposures (e.g. Reddit username hash).
    anonymous_id text,

    -- The reach event that produced this exposure.
    reach_event_id uuid REFERENCES viryaos_reach_events(id) ON DELETE CASCADE,

    -- The action that triggered this exposure.
    action_id uuid REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,

    -- The audience key (e.g. "r_MetalMusic", "signal_all").
    audience_key text NOT NULL,

    -- The channel through which the exposure happened.
    channel text NOT NULL,

    exposed_at timestamptz NOT NULL DEFAULT now(),

    -- Either fan_id or anonymous_id must be set.
    CHECK (fan_id IS NOT NULL OR anonymous_id IS NOT NULL),

    created_at timestamptz NOT NULL DEFAULT now()
);

-- Indexes for overlap computation.
CREATE INDEX IF NOT EXISTS idx_audience_exposures_workspace_audience
    ON viryaos_audience_exposures (workspace_id, audience_key);

CREATE INDEX IF NOT EXISTS idx_audience_exposures_fan
    ON viryaos_audience_exposures (workspace_id, fan_id)
    WHERE fan_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_audience_exposures_anonymous
    ON viryaos_audience_exposures (workspace_id, anonymous_id)
    WHERE anonymous_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_audience_exposures_action
    ON viryaos_audience_exposures (workspace_id, action_id)
    WHERE action_id IS NOT NULL;

-- Prevent duplicate exposures: one person can't be exposed to the same
-- reach event twice.
CREATE UNIQUE INDEX IF NOT EXISTS uq_audience_exposures_reach_identity
    ON viryaos_audience_exposures (reach_event_id, COALESCE(fan_id, '00000000-0000-0000-0000-000000000000'::uuid), COALESCE(anonymous_id, ''));
