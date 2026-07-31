-- Idempotent Bandsintown announcements generated after a source has completed
-- its initial synchronization. The first sync is a silent backfill. Later
-- publications, meaningful schedule/venue changes and cancellations can each
-- create durable, fingerprinted notifications.

CREATE TABLE event_announcements (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    kind text NOT NULL CHECK (kind IN ('published', 'updated', 'cancelled')),
    fingerprint text NOT NULL
        CHECK (btrim(fingerprint) <> '' AND char_length(fingerprint) <= 128),
    regional_recipient_count integer NOT NULL DEFAULT 0
        CHECK (regional_recipient_count >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, event_id, kind, fingerprint),
    CONSTRAINT event_announcements_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE
);

CREATE UNIQUE INDEX event_announcements_published_once_idx
    ON event_announcements (workspace_id, event_id)
    WHERE kind = 'published';

CREATE INDEX event_announcements_created_idx
    ON event_announcements (workspace_id, created_at DESC, id DESC);
