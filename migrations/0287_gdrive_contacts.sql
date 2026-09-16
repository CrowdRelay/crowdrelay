-- Google Drive contacts connector.
--
-- 1. Adds 'gdrive' to the fanbase_connections platform check — a Drive
--    connection is OAuth-backed like TikTok (encrypted token columns),
--    but it feeds contacts, not metrics: polled_by_growth_metric_sync
--    returns false for it in the domain Platform enum.
-- 2. Creates viryaos_drive_contacts — the staging table every extracted
--    address lands in. Dedup is by (workspace_id, normalized_email):
--    one row per address, period. fan_outcome and beacon_outcome are
--    independent because a beacon may also be a fan — the same email can
--    be promoted to both surfaces.

ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY[
        'meta', 'google_ads', 'reddit', 'bandsintown', 'spotify', 'youtube',
        'facebook', 'instagram', 'soundcloud', 'tiktok',
        'discord', 'telegram', 'lastfm',
        'deezer', 'discogs', 'bluesky', 'bandcamp', 'x', 'gdrive'
    ]));

CREATE TABLE viryaos_drive_contacts (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    -- The dedup key. Normalized by the domain's NormalizedEmail before write.
    normalized_email text NOT NULL CHECK (btrim(normalized_email) <> ''),
    display_name text CHECK (display_name IS NULL OR char_length(display_name) <= 200),
    organization text CHECK (organization IS NULL OR char_length(organization) <= 200),
    phone text CHECK (phone IS NULL OR char_length(phone) <= 40),
    notes text CHECK (notes IS NULL OR char_length(notes) <= 2000),
    -- What the row looks like when a type/role column names it. NULL means
    -- the file said nothing — the operator decides on promote.
    suggested_kind text CHECK (suggested_kind IS NULL OR suggested_kind IN (
        'fan', 'press', 'radio', 'playlist', 'media_patronage', 'endorsement', 'creator'
    )),
    -- Provenance: which Drive file this row last came from.
    source_file_id text NOT NULL CHECK (btrim(source_file_id) <> '' AND char_length(source_file_id) <= 200),
    source_file_name text NOT NULL CHECK (btrim(source_file_name) <> '' AND char_length(source_file_name) <= 500),
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    -- Set when a re-scanned file no longer carries this address. Never a
    -- delete — the operator decides what the disappearance means.
    disappeared_at timestamptz,
    -- Independent per destination: a contact may be a fan AND a beacon.
    fan_outcome text NOT NULL DEFAULT 'staged'
        CHECK (fan_outcome IN ('staged', 'promoted', 'dismissed')),
    beacon_outcome text NOT NULL DEFAULT 'staged'
        CHECK (beacon_outcome IN ('staged', 'promoted', 'dismissed')),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(metadata) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, normalized_email)
);

CREATE TRIGGER viryaos_drive_contacts_set_updated_at
BEFORE UPDATE ON viryaos_drive_contacts
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX viryaos_drive_contacts_review_idx
    ON viryaos_drive_contacts (workspace_id, fan_outcome, beacon_outcome, last_seen_at DESC);

CREATE INDEX viryaos_drive_contacts_file_idx
    ON viryaos_drive_contacts (workspace_id, source_file_id);

-- Per-file scan state. The Drive modifiedTime is the cheap skip signal:
-- an unchanged file costs one row read, not an export. no_email_column
-- marks files that proved not to be contact lists so the UI can say so
-- instead of silently omitting them.
CREATE TABLE viryaos_drive_files (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    file_id text NOT NULL CHECK (btrim(file_id) <> '' AND char_length(file_id) <= 200),
    file_name text NOT NULL CHECK (btrim(file_name) <> '' AND char_length(file_name) <= 500),
    mime_type text NOT NULL CHECK (btrim(mime_type) <> ''),
    last_mtime text NOT NULL DEFAULT '',
    no_email_column boolean NOT NULL DEFAULT false,
    rows_read integer NOT NULL DEFAULT 0,
    contacts_written integer NOT NULL DEFAULT 0,
    last_scanned_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, file_id)
);
