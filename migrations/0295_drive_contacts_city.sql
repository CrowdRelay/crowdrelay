-- The seed's city survives staging (1A.8).
--
-- A contacts sheet carries a City column — the extractor read it and the
-- staging table dropped it. That is the one field the booking promote
-- cannot invent: `promote_beacon_booking` is city-scoped, and today the
-- operator retypes what the file already said, or the promote refuses
-- `CityRequired` on a contact the sheet had placed. Keeping the city is
-- also what 3.10 needs — "who can help with this show" reads the staging
-- queue against a city, and a dropped column is a shortlist that cannot
-- be named.

ALTER TABLE viryaos_drive_contacts
    ADD COLUMN IF NOT EXISTS city text
    CHECK (city IS NULL OR char_length(city) <= 120);
