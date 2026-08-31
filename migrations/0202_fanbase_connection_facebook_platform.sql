-- Add 'facebook' to the fanbase_connections platform check constraint.
-- The growth metric sync worker fetches Facebook Page follower counts via
-- the Graph API and records them under platform='facebook' in the growth
-- metric series. The fanbase_connection platform is the connectable
-- surface; this migration allows connections with platform='facebook'.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;

ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY [
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown',
        'spotify', 'youtube', 'facebook'
    ]));
