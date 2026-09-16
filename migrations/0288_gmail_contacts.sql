-- Gmail contacts connector — a second intake source for the same staging
-- queue the Drive connector feeds.
--
-- 1. Adds 'gmail' to the fanbase_connections platform check — an OAuth
--    connection like gdrive, with its own consent (gmail.readonly is a
--    restricted scope; granting Drive must not silently grant Gmail).
-- 2. fanbase_connections.sync_cursor — per-connection incremental cursor.
--    Gmail's historyId lives here; Drive uses per-file mtimes instead.
-- 3. viryaos_drive_contacts.sources text[] — which intake sources have
--    produced this address. Dedup stays (workspace_id, normalized_email):
--    a contact found in both Drive and Gmail is one row with two sources.

ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY[
        'meta', 'google_ads', 'reddit', 'bandsintown', 'spotify', 'youtube',
        'facebook', 'instagram', 'soundcloud', 'tiktok',
        'discord', 'telegram', 'lastfm',
        'deezer', 'discogs', 'bluesky', 'bandcamp', 'x', 'gdrive', 'gmail'
    ]));

ALTER TABLE fanbase_connections
    ADD COLUMN IF NOT EXISTS sync_cursor text;

ALTER TABLE viryaos_drive_contacts
    ADD COLUMN IF NOT EXISTS sources text[] NOT NULL DEFAULT ARRAY['gdrive'];
ALTER TABLE viryaos_drive_contacts
    ADD CONSTRAINT viryaos_drive_contacts_sources_check
    CHECK (array_length(sources, 1) >= 1
           AND sources <@ ARRAY['gdrive', 'gmail']::text[]);
