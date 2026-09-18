-- The booking graph's missing entities (§12-5, Sprint 4V.9): the booking
-- agent, the festival edition, and the promoter–venue edge.
--
-- An agent represents the band; a promoter books the room. The intake used
-- to fold `booking_agent`/`talent_buyer` into `promoter`, which is
-- backwards: you pitch a promoter for one show, you pitch an agent to be
-- represented once — and if it works every subsequent show comes through
-- them. The pitch, the cadence and the success measure are all different,
-- and an agent is never city-scoped, so agents get their own table rather
-- than a `BookingTargetKind` variant whose `city_id NOT NULL` would lie.
--
-- A festival is a series; the edition is the schedulable object — an
-- application window that opens and closes, a lineup, a date. The composite
-- `(workspace_id, target_id)` foreign key is what makes a cross-tenant
-- attach impossible rather than merely unlikely.
--
-- One promoter works several rooms and one room hosts several promoters.
-- `viryaos_booking_targets.venue_id` (0296) stays the primary link; the edge
-- table is the many, and the read side treats a target's rooms as the union
-- of both.
CREATE TABLE viryaos_booking_agents (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name         text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 200),
    agency       text CHECK (agency IS NULL OR (btrim(agency) <> '' AND char_length(agency) <= 200)),
    contact_email text NOT NULL CHECK (char_length(contact_email) <= 320
                   AND contact_email ~* '^[^[:space:]@]+@[^[:space:]@]+\.[^[:space:]@]+$'),
    roster_url   text CHECK (roster_url IS NULL OR roster_url ~* '^https?://'),
    genres       text[] NOT NULL DEFAULT '{}',
    active       boolean NOT NULL DEFAULT true,
    -- The pitch is an application, not a negotiation: one approach per
    -- season, refusal closes the door.
    approached_at timestamptz,
    refused_until date,
    version      bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, contact_email)
);
CREATE TRIGGER viryaos_booking_agents_set_updated_at BEFORE UPDATE
    ON viryaos_booking_agents FOR EACH ROW
    EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_booking_agents_active_idx
    ON viryaos_booking_agents (workspace_id, active)
    WHERE active;

-- The edition is the schedulable object: "Brutal Assault" is the series
-- (the booking target), "Brutal Assault 2027" is the edition with an
-- application window that opens and closes. The composite foreign key means
-- an edition can only ever point at its own workspace's festival target.
CREATE TABLE viryaos_festival_editions (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id   uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_id      uuid NOT NULL,               -- the 'festival' booking target (the series)
    edition_label  text NOT NULL CHECK (btrim(edition_label) <> '' AND char_length(edition_label) <= 120),
    starts_at      timestamptz,
    application_opens_at  timestamptz,
    application_closes_at timestamptz,
    lineup_url     text CHECK (lineup_url IS NULL OR lineup_url ~* '^https?://'),
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, target_id, edition_label),
    CONSTRAINT viryaos_festival_editions_target_fk
        FOREIGN KEY (workspace_id, target_id)
        REFERENCES viryaos_booking_targets (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (application_opens_at IS NULL OR application_closes_at IS NULL
           OR application_opens_at < application_closes_at)
);
CREATE TRIGGER viryaos_festival_editions_set_updated_at BEFORE UPDATE
    ON viryaos_festival_editions FOR EACH ROW
    EXECUTE FUNCTION crowdrelay_set_updated_at();
-- The brain asks "which windows shut soon" — the close is the index's point.
CREATE INDEX viryaos_festival_editions_window_idx
    ON viryaos_festival_editions (workspace_id, application_closes_at)
    WHERE application_closes_at IS NOT NULL;

-- One promoter works several rooms; one room hosts several promoters.
-- `venue_id` references the global room — the edge is the tenant's claim
-- about who books it, the room itself belongs to everyone.
CREATE TABLE viryaos_booking_target_venues (
    workspace_id uuid NOT NULL,
    target_id    uuid NOT NULL,
    venue_id     uuid NOT NULL REFERENCES place_venues(id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, target_id, venue_id),
    CONSTRAINT viryaos_booking_target_venues_target_fk
        FOREIGN KEY (workspace_id, target_id)
        REFERENCES viryaos_booking_targets (workspace_id, id) ON DELETE CASCADE
);
CREATE INDEX viryaos_booking_target_venues_venue_idx
    ON viryaos_booking_target_venues (workspace_id, venue_id);

-- The intake's kind vocabulary gains the agent's own words: `booking_agent`
-- and `talent_buyer` are what a sheet calls the row that should land in
-- `viryaos_booking_agents` instead of folding into `promoter`. Every prior
-- value stays — a CHECK may never drop one.
ALTER TABLE viryaos_drive_contacts
    DROP CONSTRAINT IF EXISTS viryaos_drive_contacts_suggested_kind_check;
ALTER TABLE viryaos_drive_contacts
    ADD CONSTRAINT viryaos_drive_contacts_suggested_kind_check
    CHECK (suggested_kind IS NULL OR suggested_kind IN (
        'fan', 'press', 'radio', 'playlist', 'media_patronage', 'endorsement',
        'creator', 'promoter', 'venue', 'festival', 'agent', 'label',
        'booking_agent', 'talent_buyer'
    ));
