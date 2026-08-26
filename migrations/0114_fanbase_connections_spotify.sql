-- Add Spotify as a connectable fanbase platform. The original CHECK
-- allowed meta, tiktok, google_ads, reddit, bandsintown — Spotify was
-- missing despite being a first-class fan source (follower feed,
-- release watch, playlist pitching). ALTER the constraint in place.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT fanbase_connections_platform_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform IN (
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown', 'spotify'
    ));
