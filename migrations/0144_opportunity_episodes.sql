-- Opportunity episodes — the full trajectory of a fan (or audience segment)
-- from first touch to conversion (or expiry).
--
-- An episode groups a sequence of dispatches, measurements, and conversions
-- for one target audience. This is the foundation for temporal credit
-- assignment: which actions contributed to which outcomes.

CREATE TABLE IF NOT EXISTS viryaos_opportunity_episodes (
    id              TEXT NOT NULL,
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target          TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active', 'converted', 'expired')),
    started_at      TIMESTAMPTZ NOT NULL,
    ended_at        TIMESTAMPTZ,
    PRIMARY KEY (workspace_id, id)
);

CREATE INDEX idx_opportunity_episodes_workspace
    ON viryaos_opportunity_episodes(workspace_id, started_at DESC);

CREATE INDEX idx_opportunity_episodes_status
    ON viryaos_opportunity_episodes(workspace_id, status);
