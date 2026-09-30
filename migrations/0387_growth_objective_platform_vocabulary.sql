-- The objectives CHECK was born with eight platforms and never re-typed when
-- the metric-series vocabulary grew to twenty: an operator could declare a
-- target on instagram followers, watch the series record them, and have the
-- objective row itself rejected on insert. Objectives judge metric series, so
-- they take the same alphabet. Widening only ever accepts more rows; existing
-- rows validate unchanged.
ALTER TABLE growth_objectives
    DROP CONSTRAINT IF EXISTS growth_objectives_platform_check;
ALTER TABLE growth_objectives
    ADD CONSTRAINT growth_objectives_platform_check
    CHECK (platform = ANY (ARRAY[
        'spotify', 'youtube', 'bandsintown', 'social', 'website',
        'ticketing', 'signal', 'merch', 'facebook', 'instagram',
        'soundcloud', 'tiktok', 'discord', 'telegram', 'lastfm',
        'deezer', 'discogs', 'bluesky', 'bandcamp', 'x'
    ]));
