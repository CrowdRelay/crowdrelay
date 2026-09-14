-- Per-act bill records (Sprint 1G, §4a-3): `ticket_url` on events is a single
-- field shared by every act on the bill, which makes "which act's announce-
-- ment moved ticket clicks" unanswerable. `event_acts` names the bill and
-- gives each act its own tagged link; the click ledger carries the act_slug
-- it was attributed to so the answer survives the act row being edited.
CREATE TABLE event_acts (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    act_slug text NOT NULL CHECK (act_slug ~ '^[a-z0-9][a-z0-9-]{0,63}$'),
    act_name text NOT NULL CHECK (btrim(act_name) <> '' AND char_length(act_name) <= 160),
    position integer NOT NULL DEFAULT 0 CHECK (position BETWEEN 0 AND 999),
    ticket_url text CHECK (ticket_url IS NULL OR ticket_url ~* '^https://'),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, event_id, act_slug),
    CONSTRAINT event_acts_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX event_acts_event_idx
    ON event_acts (workspace_id, event_id, position, act_slug);

-- A click attributed to an act stays attributed: the slug is recorded with
-- the fact rather than resolved by join, so deleting or renaming the act row
-- later never rewrites history.
ALTER TABLE event_action_events
    ADD COLUMN act_slug text
        CHECK (act_slug IS NULL OR act_slug ~ '^[a-z0-9][a-z0-9-]{0,63}$');
