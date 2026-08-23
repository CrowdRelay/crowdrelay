-- Two things the campaign needs that the schema does not hold.
--
-- 1. `last_activity_at` on the fan row.
--
-- `fan_last_meaningful_action` is the source of truth and stays that way; this
-- column is a cache of it, refreshed by the metric cycle. It is a deliberate
-- denormalization and the justification is narrow: the KPI query calls that
-- function once per fan, and every read model that wants to sort or filter by
-- recency would do the same. At nineteen fans this changes nothing. At ten
-- thousand it is the difference between a metric cycle and a timeout.
--
-- The invalidation story is what makes it safe: it is recomputed wholesale
-- every cycle rather than maintained by triggers scattered across six tables,
-- so it can be stale by one cycle and can never be subtly wrong.
ALTER TABLE fans
    ADD COLUMN last_activity_at timestamptz;

COMMENT ON COLUMN fans.last_activity_at IS
    'Cache of fan_last_meaningful_action, refreshed each Autopilot cycle. Never the source of truth.';

-- Sorting and filtering by recency is the whole point of the column.
CREATE INDEX IF NOT EXISTS fans_last_activity_idx
    ON fans (workspace_id, last_activity_at DESC)
    WHERE last_activity_at IS NOT NULL;

-- 2. Beacon discovery that is not tied to a show.
--
-- `evaluate_beacon_discovery` only fires inside `discovery_lead_days` of an
-- upcoming event, which means a workspace with no shows booked can never
-- discover a scene node — and scene nodes are how a band gets shows in the
-- first place. The campaign needs the opposite order: find the latarniks in a
-- city because the city is warm, then play there.
--
-- `last_city_discovery_at` records when a city was last scouted, so the
-- city-scoped rule has a cooldown of its own without borrowing the event one.
CREATE TABLE viryaos_city_beacon_discovery (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    city_id uuid NOT NULL REFERENCES cities(id) ON DELETE CASCADE,
    last_discovery_at timestamptz NOT NULL DEFAULT now(),
    -- How many the last scout asked for, so a repeat can tell whether the
    -- previous attempt actually produced anything.
    requested_count integer NOT NULL DEFAULT 0 CHECK (requested_count >= 0),
    PRIMARY KEY (workspace_id, city_id)
);
