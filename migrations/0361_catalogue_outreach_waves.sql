-- A catalogue wave: the act pitching its catalogue, one wave per kind per
-- month (see `WaveAnchor::Catalogue`). The catalogue has no date of its own,
-- so the anchor is a season id and its date closes the month's wave.
ALTER TABLE outreach_waves DROP CONSTRAINT outreach_waves_anchor_kind_check;
ALTER TABLE outreach_waves ADD CONSTRAINT outreach_waves_anchor_kind_check
    CHECK (anchor_kind IN ('release', 'event', 'catalogue'));
