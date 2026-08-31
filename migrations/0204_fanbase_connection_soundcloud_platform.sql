-- Add 'soundcloud' to the fanbase_connections platform check constraint.
-- The growth metric sync worker fetches SoundCloud follower counts by
-- scraping the public artist page HTML (embedded hydration JSON).
-- No API key or app registration needed.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY[
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown',
        'spotify', 'youtube', 'facebook', 'instagram', 'soundcloud'
    ]));
