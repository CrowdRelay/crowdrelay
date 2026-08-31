-- Add 'deezer', 'discogs', 'bluesky', and 'bandcamp' to the fanbase_connections
-- and viryaos_growth_metric_series platform check constraints.
--
-- Deezer: artist fan counts are fetched from the free Deezer API
-- (api.deezer.com/artist/{id}). The operator provides the numeric Deezer
-- artist ID; no API key is needed.
--
-- Discogs: artist collection/wantlist counts are fetched via the Discogs API
-- (api.discogs.com/artists/{id}). The operator provides the numeric Discogs
-- artist ID; a shared API token (CROWDRELAY_DISCOGS_TOKEN) is used for
-- rate-limit authentication.
--
-- Bluesky: actor follower counts are fetched from the free Bluesky public API
-- (public.api.bsky.app/xrpc/app.bsky.actor.getProfile). The operator provides
-- the handle (e.g. "virya.bsky.social"); no API key is needed.
--
-- Bandcamp: supporter counts are scraped from the artist's community page
-- HTML ({artist}.bandcamp.com/community). The operator provides the Bandcamp
-- subdomain (e.g. "virya"); no API key is needed. Bandcamp has no public API
-- — the community page lists recent supporters, which we count as a growth
-- metric proxy while the band is small.

ALTER TABLE fanbase_connections
    DROP CONSTRAINT IF EXISTS fanbase_connections_platform_check;
ALTER TABLE fanbase_connections
    ADD CONSTRAINT fanbase_connections_platform_check
    CHECK (platform = ANY (ARRAY[
        'meta', 'google_ads', 'reddit', 'bandsintown', 'spotify', 'youtube',
        'facebook', 'instagram', 'soundcloud', 'tiktok',
        'discord', 'telegram', 'lastfm',
        'deezer', 'discogs', 'bluesky', 'bandcamp'
    ]));

ALTER TABLE viryaos_growth_metric_series
    DROP CONSTRAINT IF EXISTS viryaos_growth_metric_series_platform_check;
ALTER TABLE viryaos_growth_metric_series
    ADD CONSTRAINT viryaos_growth_metric_series_platform_check
    CHECK (platform = ANY (ARRAY[
        'spotify', 'youtube', 'bandsintown', 'social', 'website',
        'ticketing', 'signal', 'merch', 'facebook', 'instagram',
        'soundcloud', 'tiktok', 'discord', 'telegram', 'lastfm',
        'deezer', 'discogs', 'bluesky', 'bandcamp'
    ]));
