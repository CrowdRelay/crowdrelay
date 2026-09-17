-- The public listing endpoint resolves by share_token alone — it is the
-- bearer credential — so the lookup needs its own index, and uniqueness is
-- what keeps a (theoretical) collision from serving the wrong band.
CREATE UNIQUE INDEX viryaos_band_listings_share_token_idx
    ON viryaos_band_listings (share_token);

-- A claim the band stands behind is a count or a date — a negative count is
-- nonsense upstream accepts today, and the public page refuses the whole
-- listing over one. Bound it where the data lands.
ALTER TABLE viryaos_band_listing_claims
    ADD CONSTRAINT viryaos_band_listing_claims_value_check
    CHECK (value IS NULL OR value >= 0);
