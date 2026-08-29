-- Episode events — individual events in a fan's trajectory.
--
-- Each event is one step in the episode: a dispatch, a measurement, a
-- conversion, or an expiry. Events are ordered by `occurred_at` and
-- record what the brain did and what happened as a result.

CREATE TABLE IF NOT EXISTS viryaos_episode_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    episode_id      TEXT NOT NULL,
    kind            TEXT NOT NULL
                    CHECK (kind IN ('dispatch', 'measurement', 'conversion', 'expired')),
    template_id     TEXT,
    action_id       UUID REFERENCES viryaos_autopilot_actions(id) ON DELETE SET NULL,
    -- Signed outcome: positive = fans gained, negative = fans lost.
    observed_outcome DOUBLE PRECISION,
    occurred_at     TIMESTAMPTZ NOT NULL,
    FOREIGN KEY (workspace_id, episode_id)
        REFERENCES viryaos_opportunity_episodes(workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX idx_episode_events_episode
    ON viryaos_episode_events(workspace_id, episode_id, occurred_at);

CREATE INDEX idx_episode_events_template
    ON viryaos_episode_events(workspace_id, template_id, occurred_at DESC);
