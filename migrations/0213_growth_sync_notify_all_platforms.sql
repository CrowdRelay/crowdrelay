-- Stop the sync NOTIFY from carrying its own copy of the platform list, and
-- give discovery places the two kinds the research actually found.
--
-- 1. notify_growth_metric_sync()
--
-- The trigger fired only for 'youtube', 'spotify' and 'reddit'. Since then the
-- connectable surface grew to seventeen platforms and the worker's
-- SYNCED_PLATFORMS to fourteen, but this list never moved. Connecting
-- SoundCloud, TikTok, Instagram, Facebook, Bandcamp, Deezer, Discogs, Bluesky,
-- Last.fm, Discord or Telegram raised no NOTIFY at all, so the first sync waited
-- for the worker's next scheduled wake instead of starting at once.
--
-- The cost was bounded — the worker sleeps at most FALLBACK_SLEEP (5 minutes)
-- and re-reads due connections on every wake, so nothing was lost, only
-- delayed. The reason to fix it is the drift, not the five minutes: two lists
-- meaning the same thing, one in Rust and one in SQL, with nothing forcing them
-- to agree.
--
-- The fix removes the second list rather than syncing it. The worker's lease
-- query already filters on SYNCED_PLATFORMS, so a NOTIFY for a platform it does
-- not poll costs exactly one wakeup that finds no due work. Over-notifying is
-- cheap; under-notifying is a silent delay. Fire for every connected platform
-- and let the single Rust list decide what is actually polled.

CREATE OR REPLACE FUNCTION notify_growth_metric_sync()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    -- No platform allowlist here on purpose: the worker's SYNCED_PLATFORMS is
    -- the one source of truth for what gets polled, and it filters the lease
    -- query. See scripts/test_platform_vocabulary_contract.py, which fails if
    -- an allowlist reappears in this function.
    IF NEW.status = 'connected'
       AND (
           TG_OP = 'INSERT'
           OR OLD.status IS DISTINCT FROM NEW.status
           OR OLD.provider_account_id IS DISTINCT FROM NEW.provider_account_id
       )
    THEN
        PERFORM pg_notify('growth_metric_sync', NEW.platform);
    END IF;
    RETURN NEW;
END;
$$;

-- 2. discovery_places.place_kind
--
-- The community research found ten Telegram channels and five Lemmy
-- communities alongside the subreddits, Discord servers and forums. Neither
-- kind existed in the vocabulary, so both would have had to land as 'other' and
-- lose the one column that says how to reach them.
--
-- Widening a CHECK is safe in the direction: ADD CONSTRAINT validates existing
-- rows, and every existing value is carried over below.
ALTER TABLE discovery_places
    DROP CONSTRAINT IF EXISTS discovery_places_place_kind_check;
ALTER TABLE discovery_places
    ADD CONSTRAINT discovery_places_place_kind_check
    CHECK (place_kind = ANY (ARRAY[
        'subreddit', 'discord', 'telegram', 'lemmy', 'forum',
        'facebook_group', 'instagram', 'tiktok', 'youtube',
        'playlist', 'zine', 'festival', 'other'
    ]));
