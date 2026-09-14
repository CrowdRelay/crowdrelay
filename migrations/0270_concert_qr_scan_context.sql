-- Per-show scan instrumentation (Sprint 1G, §4e-7.5): the scan rate is the
-- master variable of the whole aggregation argument, so every campaign
-- records the context the rate is later read against — where the QR lived,
-- whether it was announced from the stage, and what the scan offered. All
-- fields are optional and mutable because they describe what actually
-- happened, not what was planned: an operator updates them as the night
-- unfolds, and a campaign with none set is simply an unrecorded night.
ALTER TABLE concert_qr_campaigns
    ADD COLUMN placement text
        CHECK (placement IS NULL OR char_length(placement) <= 128),
    ADD COLUMN announced_from_stage boolean NOT NULL DEFAULT false,
    ADD COLUMN incentive text
        CHECK (incentive IS NULL OR char_length(incentive) <= 256);
