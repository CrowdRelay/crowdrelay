-- Phase 3: public event discovery, durable interest registration, tracked event
-- actions and reminder scheduling for the Virya website integration.

ALTER TABLE events
    ADD COLUMN description text,
    ADD COLUMN venue_address text,
    ADD COLUMN timezone text NOT NULL DEFAULT 'Europe/Warsaw'
        CHECK (btrim(timezone) <> '' AND char_length(timezone) <= 128),
    ADD COLUMN ends_at timestamptz,
    ADD COLUMN listen_url text CHECK (listen_url IS NULL OR listen_url ~* '^https?://'),
    ADD COLUMN image_url text CHECK (image_url IS NULL OR image_url ~* '^https?://'),
    ADD COLUMN trailer_url text CHECK (trailer_url IS NULL OR trailer_url ~* '^https?://'),
    ADD COLUMN external_event_url text
        CHECK (external_event_url IS NULL OR external_event_url ~* '^https?://'),
    ADD COLUMN published_at timestamptz;

UPDATE events
SET published_at = COALESCE(published_at, created_at)
WHERE status = 'published';

ALTER TABLE events
    ADD CONSTRAINT events_ends_after_start_check
        CHECK (ends_at IS NULL OR ends_at >= starts_at),
    ADD CONSTRAINT events_published_timestamp_check
        CHECK (status <> 'published' OR published_at IS NOT NULL);

CREATE INDEX events_public_schedule_idx
    ON events (workspace_id, starts_at, id)
    WHERE status = 'published';

ALTER TABLE event_interests
    ADD COLUMN id uuid NOT NULL DEFAULT gen_random_uuid(),
    ADD COLUMN source text NOT NULL DEFAULT 'event_page'
        CHECK (btrim(source) <> '' AND char_length(source) <= 128),
    ADD COLUMN anonymous_visitor_id uuid,
    ADD COLUMN referral_code_id uuid,
    ADD COLUMN request_id text
        CHECK (request_id IS NULL OR (btrim(request_id) <> '' AND char_length(request_id) <= 128)),
    ADD COLUMN updated_at timestamptz NOT NULL DEFAULT now(),
    ADD CONSTRAINT event_interests_workspace_id_unique UNIQUE (workspace_id, id),
    ADD CONSTRAINT event_interests_referral_code_fk
        FOREIGN KEY (workspace_id, referral_code_id)
        REFERENCES referral_codes (workspace_id, id)
        ON DELETE RESTRICT;

CREATE TRIGGER event_interests_set_updated_at
BEFORE UPDATE ON event_interests
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX event_interests_fan_time_idx
    ON event_interests (workspace_id, fan_id, created_at DESC, event_id);
CREATE INDEX event_interests_event_time_idx
    ON event_interests (workspace_id, event_id, created_at DESC, fan_id);
CREATE INDEX event_interests_campaign_idx
    ON event_interests (workspace_id, campaign_id, created_at DESC)
    WHERE campaign_id IS NOT NULL;

CREATE TABLE event_action_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    action text NOT NULL CHECK (
        action IN ('page_view', 'ticket_click', 'calendar_download', 'listen_click', 'share_click')
    ),
    campaign_id uuid,
    anonymous_visitor_id uuid,
    referrer_host text,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT event_action_events_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT event_action_events_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES campaigns (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX event_action_events_event_time_idx
    ON event_action_events (workspace_id, event_id, action, occurred_at DESC);
CREATE INDEX event_action_events_retention_idx
    ON event_action_events (occurred_at);

CREATE TABLE event_reminder_jobs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    reminder_kind text NOT NULL
        CHECK (btrim(reminder_kind) <> '' AND char_length(reminder_kind) <= 64),
    due_at timestamptz NOT NULL,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'enqueued', 'cancelled')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    enqueued_at timestamptz,
    cancelled_at timestamptz,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, event_id, fan_id, reminder_kind),
    CONSTRAINT event_reminder_jobs_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT event_reminder_jobs_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (
        (status = 'pending' AND enqueued_at IS NULL AND cancelled_at IS NULL)
        OR (status = 'enqueued' AND enqueued_at IS NOT NULL AND cancelled_at IS NULL)
        OR (status = 'cancelled' AND cancelled_at IS NOT NULL)
    )
);

CREATE TRIGGER event_reminder_jobs_set_updated_at
BEFORE UPDATE ON event_reminder_jobs
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX event_reminder_jobs_due_idx
    ON event_reminder_jobs (due_at, id)
    WHERE status = 'pending';
