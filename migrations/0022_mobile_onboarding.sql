-- Moderated city requests and proximity-based concert notifications.
ALTER TABLE cities
    ADD COLUMN IF NOT EXISTS moderation_status text NOT NULL DEFAULT 'approved'
        CHECK (moderation_status IN ('pending', 'approved', 'merged', 'rejected')),
    ADD COLUMN IF NOT EXISTS request_count integer NOT NULL DEFAULT 0
        CHECK (request_count >= 0),
    ADD COLUMN IF NOT EXISTS first_requested_at timestamptz,
    ADD COLUMN IF NOT EXISTS last_requested_at timestamptz;

CREATE INDEX IF NOT EXISTS cities_moderation_lookup_idx
    ON cities (moderation_status, country_code, name, id);

CREATE TABLE fan_location_preferences (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    city_id uuid NOT NULL REFERENCES cities(id) ON DELETE RESTRICT,
    nearby_gigs_enabled boolean NOT NULL DEFAULT true,
    radius_km integer NOT NULL DEFAULT 150 CHECK (radius_km BETWEEN 25 AND 500),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, fan_id),
    CONSTRAINT fan_location_preferences_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);

CREATE TRIGGER fan_location_preferences_set_updated_at
BEFORE UPDATE ON fan_location_preferences
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX fan_location_preferences_city_idx
    ON fan_location_preferences (workspace_id, city_id)
    WHERE nearby_gigs_enabled;

CREATE TABLE nearby_gig_notifications (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    event_id uuid NOT NULL,
    distance_km integer NOT NULL CHECK (distance_km BETWEEN 0 AND 20000),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, fan_id, event_id),
    CONSTRAINT nearby_gig_notifications_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT nearby_gig_notifications_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE
);
