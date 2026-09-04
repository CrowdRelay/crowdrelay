-- Attempt state for automatic city geocoding.
--
-- A city a fan requests carries a name and no coordinates, and proximity
-- delivery needs them on both ends, so every fan sitting in one is unreachable
-- until somebody supplies them. Nothing did: the geocode endpoint had no
-- caller. These columns let a worker do it without ever asking twice for an
-- answer it already has, and without retrying a name that will never resolve.
--
-- The `cities` row is the cache. Once latitude and longitude are set the
-- selection skips the row entirely, so a resolved city is never looked up
-- again and no separate cache table is needed.

ALTER TABLE cities
    ADD COLUMN IF NOT EXISTS geocode_attempts integer NOT NULL DEFAULT 0
        CHECK (geocode_attempts >= 0),
    ADD COLUMN IF NOT EXISTS geocode_last_attempt_at timestamptz,
    -- When the next attempt becomes eligible. NULL means "eligible now", which
    -- is the correct state for every city that already existed.
    ADD COLUMN IF NOT EXISTS geocode_next_attempt_at timestamptz,
    -- Why the last attempt produced nothing. Kept for the operator queue: a
    -- city that has exhausted its attempts needs a human, and the reason is
    -- what tells them whether to fix the name or add it by hand.
    ADD COLUMN IF NOT EXISTS geocode_last_error text;

-- The worker's claim: cities with no coordinates whose backoff has elapsed,
-- most-requested first. Partial, because a geocoded city must never be scanned
-- again -- that is what makes the work proportional to what is unresolved
-- rather than to the size of the catalogue.
CREATE INDEX IF NOT EXISTS cities_geocode_pending_idx
    ON cities (geocode_next_attempt_at, request_count DESC, id)
    WHERE latitude IS NULL
      AND moderation_status IN ('pending', 'approved');
