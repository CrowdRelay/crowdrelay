-- Add 'facebook' to the fanbase_connections and viryaos_growth_metric_series
-- platform check constraints. The growth metric sync worker fetches Facebook
-- Page follower counts via the Graph API and records them under
-- platform='facebook' in the growth metric series.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;

ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY [
        'meta', 'tiktok', 'google_ads', 'reddit', 'bandsintown',
        'spotify', 'youtube', 'facebook'
    ]));

ALTER TABLE viryaos_growth_metric_series
    DROP CONSTRAINT IF EXISTS viryaos_growth_metric_series_platform_check;

ALTER TABLE viryaos_growth_metric_series
    ADD CONSTRAINT viryaos_growth_metric_series_platform_check
    CHECK (platform = ANY (ARRAY [
        'spotify', 'youtube', 'bandsintown', 'social', 'website',
        'ticketing', 'signal', 'merch', 'facebook'
    ]));
