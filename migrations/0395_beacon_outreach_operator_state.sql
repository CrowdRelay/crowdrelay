-- Operator state on the Beacon relationship ledger.
--
-- A booker who sees a recommended outreach can defer it ("not now") or
-- decline it ("not this pair"), and the next evaluation cycle must respect
-- that answer instead of re-offering the same ask. `status = 'declined'`
-- already suppressed a pair; what was missing is who declined it and the
-- "not now" state between candidate and declined.
--
-- `deferred_until` holds a pair out of the due set until the timestamp
-- passes; the campaign row stays 'candidate' and becomes eligible again on
-- its own. `declined_via` separates an operator's "not this one" from a
-- partner's "no" — the two answers carry different weight in the
-- relationship history and must not read as the same signal.

ALTER TABLE beacon_campaigns
    ADD COLUMN IF NOT EXISTS deferred_until timestamptz;

ALTER TABLE beacon_campaigns
    ADD COLUMN IF NOT EXISTS declined_via text;

ALTER TABLE beacon_campaigns
    DROP CONSTRAINT IF EXISTS beacon_campaigns_declined_via_check;
ALTER TABLE beacon_campaigns
    ADD CONSTRAINT beacon_campaigns_declined_via_check
    CHECK (declined_via IS NULL OR declined_via IN (
        'partner_reply', 'operator', 'system'
    ));

-- Declined_via is only meaningful on a declined row; elsewhere it is noise
-- that later reads would have to disambiguate anyway.
ALTER TABLE beacon_campaigns
    DROP CONSTRAINT IF EXISTS beacon_campaigns_declined_via_consistent;
ALTER TABLE beacon_campaigns
    ADD CONSTRAINT beacon_campaigns_declined_via_consistent
    CHECK (declined_via IS NULL OR status = 'declined');

-- A defer that outlives the show itself is an expired note, not a guard.
-- The snapshot loader also bounds it to the event window so a stale defer
-- on a passed show cannot suppress next year's ask.
ALTER TABLE beacon_campaigns
    DROP CONSTRAINT IF EXISTS beacon_campaigns_deferred_until_sane;
ALTER TABLE beacon_campaigns
    ADD CONSTRAINT beacon_campaigns_deferred_until_sane
    CHECK (deferred_until IS NULL OR deferred_until > '2020-01-01'::timestamptz);
