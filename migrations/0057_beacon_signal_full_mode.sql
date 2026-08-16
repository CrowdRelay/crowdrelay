-- Complete Signal Latarnik lifecycle on top of the v1 Beacon access foundation.
--
-- `viryaos_beacons` remains the relationship/consent source of truth. These
-- tables capture Signal-channel state, per-event engagement, press assets and
-- coverage without creating a second CRM.

ALTER TABLE viryaos_beacon_signal_profiles
    ADD COLUMN invite_count integer NOT NULL DEFAULT 0 CHECK (invite_count BETWEEN 0 AND 10000),
    ADD COLUMN last_invited_at timestamptz,
    ADD COLUMN paused_at timestamptz,
    ADD COLUMN revoked_at timestamptz;

UPDATE viryaos_beacon_signal_profiles
SET invite_count = CASE WHEN joined_at IS NOT NULL OR invite_expires_at IS NOT NULL THEN 1 ELSE 0 END,
    last_invited_at = COALESCE(invite_expires_at - interval '14 days', created_at)
WHERE invite_count = 0;

ALTER TABLE viryaos_beacon_press_requests
    ADD COLUMN resolution_note text CHECK (
        resolution_note IS NULL OR (btrim(resolution_note) <> '' AND char_length(resolution_note) <= 2000)
    ),
    ADD COLUMN updated_at timestamptz NOT NULL DEFAULT now();

ALTER TABLE viryaos_beacon_press_requests
    DROP CONSTRAINT IF EXISTS viryaos_beacon_press_requests_event_id_fkey;
ALTER TABLE viryaos_beacon_press_requests
    ADD CONSTRAINT viryaos_beacon_press_requests_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id) ON DELETE SET NULL (event_id);

ALTER TABLE viryaos_beacon_press_requests
    DROP CONSTRAINT IF EXISTS viryaos_beacon_press_requests_check;
ALTER TABLE viryaos_beacon_press_requests
    ADD CONSTRAINT viryaos_beacon_press_requests_resolution_check CHECK (
        (status = 'open' AND resolved_at IS NULL)
        OR (status IN ('resolved','cancelled') AND resolved_at IS NOT NULL)
    );

CREATE TRIGGER viryaos_beacon_press_requests_set_updated_at
BEFORE UPDATE ON viryaos_beacon_press_requests
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_beacon_signal_event_engagements (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    beacon_id uuid NOT NULL,
    event_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'eligible' CHECK (status IN (
        'eligible','notified','opened','interested','helping','completed','declined'
    )),
    help_kind text CHECK (help_kind IS NULL OR help_kind IN (
        'article','radio','podcast','photos','share','contact','other'
    )),
    help_details text CHECK (
        help_details IS NULL OR (btrim(help_details) <> '' AND char_length(help_details) <= 1500)
    ),
    notification_count integer NOT NULL DEFAULT 0 CHECK (notification_count BETWEEN 0 AND 100),
    first_notified_at timestamptz,
    last_notified_at timestamptz,
    first_opened_at timestamptz,
    last_opened_at timestamptz,
    interested_at timestamptz,
    helping_at timestamptz,
    completed_at timestamptz,
    declined_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, beacon_id, event_id),
    CONSTRAINT viryaos_beacon_signal_event_engagements_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_beacon_signal_event_engagements_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id) ON DELETE CASCADE,
    CHECK (status <> 'helping' OR help_kind IS NOT NULL),
    CHECK (first_notified_at IS NULL OR last_notified_at IS NOT NULL),
    CHECK (notification_count = 0 OR first_notified_at IS NOT NULL)
);
CREATE INDEX viryaos_beacon_signal_event_engagements_status_idx
    ON viryaos_beacon_signal_event_engagements
       (workspace_id, status, last_notified_at, event_id, beacon_id);
CREATE INDEX viryaos_beacon_signal_event_engagements_beacon_idx
    ON viryaos_beacon_signal_event_engagements
       (workspace_id, beacon_id, updated_at DESC, event_id);
CREATE TRIGGER viryaos_beacon_signal_event_engagements_set_updated_at
BEFORE UPDATE ON viryaos_beacon_signal_event_engagements
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_beacon_signal_coverage (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    beacon_id uuid NOT NULL,
    event_id uuid NOT NULL,
    coverage_kind text NOT NULL CHECK (coverage_kind IN (
        'article','radio','video','photo','social','podcast','other'
    )),
    url text NOT NULL CHECK (char_length(url) BETWEEN 12 AND 2048 AND url ~* '^https://'),
    title text CHECK (title IS NULL OR (btrim(title) <> '' AND char_length(title) <= 240)),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_beacon_signal_coverage_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_beacon_signal_coverage_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id) ON DELETE CASCADE,
    UNIQUE (workspace_id, beacon_id, event_id, url)
);
CREATE INDEX viryaos_beacon_signal_coverage_recent_idx
    ON viryaos_beacon_signal_coverage (workspace_id, created_at DESC, id DESC);

CREATE TABLE viryaos_beacon_press_assets (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid,
    asset_key text NOT NULL CHECK (asset_key ~ '^[a-z0-9][a-z0-9_-]{1,63}$'),
    asset_kind text NOT NULL CHECK (asset_kind IN (
        'epk','photo','logo','bio','audio','video','rider','social','contact','link'
    )),
    label_pl text NOT NULL CHECK (btrim(label_pl) <> '' AND char_length(label_pl) <= 120),
    label_en text NOT NULL CHECK (btrim(label_en) <> '' AND char_length(label_en) <= 120),
    url text NOT NULL CHECK (
        char_length(url) BETWEEN 8 AND 2048
        AND (url ~* '^https://' OR url ~* '^mailto:')
    ),
    active boolean NOT NULL DEFAULT true,
    sort_order integer NOT NULL DEFAULT 100 CHECK (sort_order BETWEEN 0 AND 10000),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_beacon_press_assets_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id) ON DELETE CASCADE,
    UNIQUE NULLS NOT DISTINCT (workspace_id, asset_key, event_id)
);
CREATE INDEX viryaos_beacon_press_assets_active_idx
    ON viryaos_beacon_press_assets (workspace_id, event_id, sort_order, asset_key)
    WHERE active;
CREATE TRIGGER viryaos_beacon_press_assets_set_updated_at
BEFORE UPDATE ON viryaos_beacon_press_assets
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

INSERT INTO viryaos_beacon_press_assets
    (workspace_id, asset_key, asset_kind, label_pl, label_en, url, sort_order)
SELECT id, seed.asset_key, seed.asset_kind, seed.label_pl, seed.label_en, seed.url, seed.sort_order
FROM workspaces
CROSS JOIN (VALUES
    ('epk', 'epk', 'EPK', 'EPK', 'https://virya.music/pl/epk', 10),
    ('gallery', 'photo', 'Zdjęcia', 'Photos', 'https://virya.music/gallery', 20),
    ('rider', 'rider', 'Rider', 'Rider', 'https://virya.music/techrider.pdf', 30),
    ('spotify', 'audio', 'Spotify', 'Spotify', 'https://open.spotify.com/artist/6bbW0jOKAWJWm3h6CTWaAS', 40),
    ('youtube', 'video', 'YouTube', 'YouTube', 'https://www.youtube.com/@ViryaOfficial', 50),
    ('contact', 'contact', 'Kontakt', 'Contact', 'mailto:virya.crew@gmail.com', 60)
) AS seed(asset_key, asset_kind, label_pl, label_en, url, sort_order)
ON CONFLICT (workspace_id, asset_key, event_id) DO NOTHING;

INSERT INTO viryaos_manager_config (workspace_id, config_key, value)
SELECT id, 'beacon_signal_full_policy', jsonb_build_object(
    'wave_size', 20,
    'lead_days', 60,
    'max_wave_size', 100,
    'max_batch_invites', 200,
    'default_radius_km', 100,
    'maximum_radius_km', 500,
    'session_ttl_days', 180
) FROM workspaces
ON CONFLICT (workspace_id, config_key) DO NOTHING;
