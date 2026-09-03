-- Add 'x_account' to the discovery_places place_kind CHECK constraint.
-- The X discovery worker searches X (Twitter) for relevant accounts (music
-- curators, genre communities, artists) and upserts them as places in the
-- Audience Graph. X accounts are public profiles — they feed the graph as
-- signals, not the fans table as PII.
ALTER TABLE discovery_places
    DROP CONSTRAINT IF EXISTS discovery_places_place_kind_check;
ALTER TABLE discovery_places
    ADD CONSTRAINT discovery_places_place_kind_check
    CHECK (place_kind = ANY (ARRAY[
        'subreddit', 'discord', 'telegram', 'lemmy', 'forum',
        'facebook_group', 'instagram', 'tiktok', 'youtube',
        'playlist', 'zine', 'festival', 'x_account', 'other'
    ]));
