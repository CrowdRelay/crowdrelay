-- Signal Latarnik / Beacon mode.
--
-- Beacons remain the authoritative relationship records. This migration adds a
-- revocable Signal access layer, preferences, press-material requests and a
-- dedicated push audience without turning Beacons into fans or staff users.

CREATE TABLE viryaos_beacon_signal_profiles (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    beacon_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'invited' CHECK (status IN ('invited','active','paused','revoked')),
    invite_token_hash bytea CHECK (invite_token_hash IS NULL OR octet_length(invite_token_hash) = 32),
    invite_expires_at timestamptz,
    radius_km integer NOT NULL DEFAULT 100 CHECK (radius_km BETWEEN 10 AND 500),
    locale text NOT NULL DEFAULT 'pl' CHECK (locale ~ '^[a-z]{2}(-[A-Z]{2})?$'),
    topics text[] NOT NULL DEFAULT ARRAY['shows','press_materials']::text[],
    nearby_gigs_enabled boolean NOT NULL DEFAULT true,
    joined_at timestamptz,
    last_seen_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, beacon_id),
    CONSTRAINT viryaos_beacon_signal_profiles_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE,
    CHECK ((status = 'invited') = (invite_token_hash IS NOT NULL)),
    CHECK ((invite_token_hash IS NULL) = (invite_expires_at IS NULL)),
    CHECK (invite_expires_at IS NULL OR invite_expires_at > created_at)
);
CREATE UNIQUE INDEX viryaos_beacon_signal_profiles_invite_uq
    ON viryaos_beacon_signal_profiles (workspace_id, invite_token_hash)
    WHERE invite_token_hash IS NOT NULL;
CREATE INDEX viryaos_beacon_signal_profiles_active_idx
    ON viryaos_beacon_signal_profiles (workspace_id, beacon_id, radius_km)
    WHERE status = 'active' AND nearby_gigs_enabled;
CREATE TRIGGER viryaos_beacon_signal_profiles_set_updated_at
BEFORE UPDATE ON viryaos_beacon_signal_profiles
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_beacon_signal_sessions (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    id uuid NOT NULL DEFAULT gen_random_uuid(),
    beacon_id uuid NOT NULL,
    token_hash bytea NOT NULL CHECK (octet_length(token_hash) = 32),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, token_hash),
    CONSTRAINT viryaos_beacon_signal_sessions_profile_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacon_signal_profiles (workspace_id, beacon_id) ON DELETE CASCADE,
    CHECK (expires_at > created_at)
);
CREATE INDEX viryaos_beacon_signal_sessions_active_idx
    ON viryaos_beacon_signal_sessions (workspace_id, beacon_id, expires_at DESC, id)
    WHERE revoked_at IS NULL;

CREATE TABLE viryaos_beacon_press_requests (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    beacon_id uuid NOT NULL,
    event_id uuid REFERENCES events(id) ON DELETE SET NULL,
    request_kind text NOT NULL CHECK (request_kind IN (
        'press_photo','wav','clean_version','interview','accreditation','custom'
    )),
    details text CHECK (details IS NULL OR (btrim(details) <> '' AND char_length(details) <= 1500)),
    status text NOT NULL DEFAULT 'open' CHECK (status IN ('open','resolved','cancelled')),
    created_at timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    CONSTRAINT viryaos_beacon_press_requests_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE,
    CHECK ((status = 'resolved') = (resolved_at IS NOT NULL))
);
CREATE INDEX viryaos_beacon_press_requests_open_idx
    ON viryaos_beacon_press_requests (workspace_id, created_at, id)
    WHERE status = 'open';

-- Reuse the proven push transport/ACK pipeline. The principal hash for a Beacon
-- endpoint is the hash of a currently valid Beacon Signal device session.
ALTER TABLE fan_push_endpoints
    DROP CONSTRAINT IF EXISTS fan_push_endpoints_audience_check;
ALTER TABLE fan_push_endpoints
    ADD CONSTRAINT fan_push_endpoints_audience_check CHECK (
        (audience_kind = 'fan' AND fan_id IS NOT NULL AND principal_hash IS NULL)
        OR
        (audience_kind IN ('staff','beacon') AND fan_id IS NULL AND principal_hash IS NOT NULL
            AND octet_length(principal_hash) = 32)
    );
CREATE INDEX fan_push_endpoints_active_beacon_idx
    ON fan_push_endpoints (workspace_id, principal_hash, transport, id)
    WHERE audience_kind = 'beacon' AND active AND invalidated_at IS NULL;

ALTER TABLE fan_push_deliveries
    DROP CONSTRAINT IF EXISTS fan_push_deliveries_source_kind_check;
ALTER TABLE fan_push_deliveries
    ADD CONSTRAINT fan_push_deliveries_source_kind_check CHECK (
        source_kind IN ('nearby_concert', 'communication_campaign', 'show_checklist', 'beacon_nearby_concert')
    );
ALTER TABLE fan_push_deliveries
    DROP CONSTRAINT IF EXISTS fan_push_deliveries_audience_check;
ALTER TABLE fan_push_deliveries
    ADD CONSTRAINT fan_push_deliveries_audience_check CHECK (
        (audience_kind = 'fan' AND fan_id IS NOT NULL)
        OR
        (audience_kind IN ('staff','beacon') AND fan_id IS NULL)
    );
CREATE INDEX fan_push_deliveries_beacon_recent_idx
    ON fan_push_deliveries (workspace_id, created_at DESC, id DESC)
    WHERE audience_kind = 'beacon';

INSERT INTO viryaos_manager_config (workspace_id, config_key, value)
SELECT id, 'beacon_signal_policy', jsonb_build_object(
    'wave_size', 20,
    'default_radius_km', 100,
    'maximum_radius_km', 500,
    'session_ttl_days', 180,
    'press_room_path_pl', '/pl/latarnik',
    'press_room_path_en', '/latarnik'
) FROM workspaces
ON CONFLICT (workspace_id, config_key) DO NOTHING;
