-- Reach conversions — one-to-many from reach events to fan conversions.
--
-- A broadcast reach event (e.g. a Reddit post to 10,000 subscribers) can
-- produce multiple fan conversions. The existing `viryaos_reach_events` row
-- has a single `converted_fan_id` column, which is wrong for broadcasts:
-- one Reddit post → 7 fans, but the schema only records one UUID.
--
-- This table separates the reach attempt (the `viryaos_reach_events` row)
-- from the individual conversions. Each conversion is a separate row linked
-- to the reach event by `reach_event_id`.
--
-- Design:
-- - `reach_event_id` links back to the reach attempt.
-- - `fan_id` links to the fan who converted (optional — some conversions
--   are anonymous until the fan is created).
-- - `incremental` marks whether this conversion was incremental (wouldn't
--   have happened without the action). This is set by the attribution model.
-- - `durable_30d` is set after the 30-day observation window: was this fan
--   still active after 30 days?

CREATE TABLE IF NOT EXISTS viryaos_reach_conversions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- The reach attempt that produced this conversion.
    reach_event_id uuid NOT NULL REFERENCES viryaos_reach_events(id) ON DELETE CASCADE,

    -- The fan who converted (optional — may be null for anonymous conversions).
    fan_id uuid REFERENCES fans(id) ON DELETE SET NULL,

    -- When the conversion was observed.
    converted_at timestamptz NOT NULL DEFAULT now(),

    -- Whether this conversion was incremental (wouldn't have happened without
    -- the action). Set by the attribution model, not at conversion time.
    incremental boolean NOT NULL DEFAULT false,

    -- Whether this fan is still active after 30 days. Set by the durability
    -- measurement loop after the 30-day window elapses.
    durable_30d boolean,

    created_at timestamptz NOT NULL DEFAULT now()
);

-- Indexes for the brain's most common queries.
CREATE INDEX IF NOT EXISTS idx_reach_conversions_workspace_converted_at
    ON viryaos_reach_conversions (workspace_id, converted_at DESC);

CREATE INDEX IF NOT EXISTS idx_reach_conversions_reach_event
    ON viryaos_reach_conversions (reach_event_id);

CREATE INDEX IF NOT EXISTS idx_reach_conversions_fan
    ON viryaos_reach_conversions (workspace_id, fan_id)
    WHERE fan_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_reach_conversions_incremental
    ON viryaos_reach_conversions (workspace_id, incremental)
    WHERE incremental = true;

CREATE INDEX IF NOT EXISTS idx_reach_conversions_durable
    ON viryaos_reach_conversions (workspace_id, durable_30d)
    WHERE durable_30d IS NOT NULL;

-- Ensure one conversion per (reach_event_id, fan_id) — the same fan can't
-- convert twice from the same reach event.
CREATE UNIQUE INDEX IF NOT EXISTS uq_reach_conversions_event_fan
    ON viryaos_reach_conversions (reach_event_id, fan_id)
    WHERE fan_id IS NOT NULL;
