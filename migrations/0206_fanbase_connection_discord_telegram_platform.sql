-- Add 'discord', 'telegram', and 'lastfm' to the fanbase_connections and
-- viryaos_growth_metric_series platform check constraints.
--
-- Discord: server member counts are fetched from disdex.io (free, no API key).
-- The operator provides a Discord server ID; the sync worker fetches the
-- member count from https://disdex.io/api/v1/servers/{id}.
--
-- Telegram: channel subscriber counts are fetched via the Bot API
-- (getChatMemberCount). The operator provides a channel username and a
-- bot token; the token is encrypted and stored in fanbase_connections.
--
-- Last.fm: artist listener counts are fetched via the official Last.fm API
-- (artist.getInfo). The operator provides the artist name; the API key
-- is stored as an env var (CROWDRELAY_LASTFM_API_KEY).

-- The connection list carries forward every value 0113–0204 allowed. 'meta'
-- and 'google_ads' stay: ad_conversion.rs still reads meta connections, and
-- ADD CONSTRAINT validates existing rows, so dropping a live value here would
-- abort the migration on any deployment that has one.
ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY[
        'meta', 'google_ads', 'reddit', 'bandsintown', 'spotify', 'youtube',
        'facebook', 'instagram', 'soundcloud', 'tiktok',
        'discord', 'telegram', 'lastfm'
    ]));

ALTER TABLE viryaos_growth_metric_series
    DROP CONSTRAINT IF EXISTS viryaos_growth_metric_series_platform_check;
ALTER TABLE viryaos_growth_metric_series
    ADD CONSTRAINT viryaos_growth_metric_series_platform_check
    CHECK (platform = ANY (ARRAY[
        'spotify', 'youtube', 'bandsintown', 'social', 'website',
        'ticketing', 'signal', 'merch', 'facebook', 'instagram',
        'soundcloud', 'tiktok', 'discord', 'telegram', 'lastfm'
    ]));
