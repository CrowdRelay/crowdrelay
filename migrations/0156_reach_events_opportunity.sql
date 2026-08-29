-- Link reach events to opportunity and episode.
--
-- Reach events currently have `action_id` but no `opportunity_id` or
-- `episode_id` link. The brain can't trace a reach event back to the
-- opportunity that motivated it, or to the episode it belongs to.
--
-- This migration adds `opportunity_id` and `episode_id` columns to
-- `viryaos_reach_events` and indexes them for the brain's queries.

ALTER TABLE viryaos_reach_events
    ADD COLUMN IF NOT EXISTS opportunity_id text,
    ADD COLUMN IF NOT EXISTS episode_id text;

-- Index for querying reach events by opportunity.
CREATE INDEX IF NOT EXISTS idx_reach_events_opportunity
    ON viryaos_reach_events (workspace_id, opportunity_id)
    WHERE opportunity_id IS NOT NULL;

-- Index for querying reach events by episode.
CREATE INDEX IF NOT EXISTS idx_reach_events_episode
    ON viryaos_reach_events (workspace_id, episode_id)
    WHERE episode_id IS NOT NULL;
