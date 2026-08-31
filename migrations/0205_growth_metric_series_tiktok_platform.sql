-- Add 'tiktok' to the viryaos_growth_metric_series platform check
-- constraint. The growth metric sync worker fetches TikTok creator
-- follower counts via the Display API /v2/user/info/ endpoint using
-- OAuth tokens stored in fanbase_connections.credential_ref.
-- 'tiktok' is already in the fanbase_connections check constraint.
ALTER TABLE viryaos_growth_metric_series
    DROP CONSTRAINT IF EXISTS viryaos_growth_metric_series_platform_check;
ALTER TABLE viryaos_growth_metric_series
    ADD CONSTRAINT viryaos_growth_metric_series_platform_check
    CHECK (platform = ANY (ARRAY[
        'spotify', 'youtube', 'bandsintown', 'social', 'website',
        'ticketing', 'signal', 'merch', 'facebook', 'instagram',
        'soundcloud', 'tiktok'
    ]));
