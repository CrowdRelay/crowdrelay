-- Add 'instagram' to the fanbase_connections and viryaos_growth_metric_series
-- platform check constraints. The growth metric sync worker fetches Instagram
-- professional account follower counts via the Graph API (using the same
-- Facebook Page token — the IG Business account is linked to the Page).
ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;

ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY [
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown',
        'spotify', 'youtube', 'facebook', 'instagram'
    ]));

ALTER TABLE viryaos_growth_metric_series
    DROP CONSTRAINT IF EXISTS viryaos_growth_metric_series_platform_check;

ALTER TABLE viryaos_growth_metric_series
    ADD CONSTRAINT viryaos_growth_metric_series_platform_check
    CHECK (platform = ANY (ARRAY [
        'spotify', 'youtube', 'bandsintown', 'social', 'website',
        'ticketing', 'signal', 'merch', 'facebook', 'instagram'
    ]));
