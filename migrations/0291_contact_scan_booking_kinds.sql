-- Contact-scan booking kinds (1A.5).
--
-- A sent folder holds promoters, venues and festival organisers the band
-- actually dealt with — but `suggested_kind` could only name the outreach
-- vocabulary (press/radio/playlist/...), so a spreadsheet column reading
-- "promoter" silently classified nothing and the promote path had no kind
-- to carry. Those contacts are booking supply, not press outreach:
-- `viryaos_booking_targets` already owns the venue/promoter/festival
-- vocabulary, so this widens only the staging side — the promote handler
-- routes a booking kind into `viryaos_booking_candidates` (admitted on
-- first-party grounds: the band's own sent mail is the evidence, not a
-- discovered route) rather than `agent_outreach_targets`.

ALTER TABLE viryaos_drive_contacts
    DROP CONSTRAINT IF EXISTS viryaos_drive_contacts_suggested_kind_check;
ALTER TABLE viryaos_drive_contacts
    ADD CONSTRAINT viryaos_drive_contacts_suggested_kind_check
    CHECK (suggested_kind IS NULL OR suggested_kind IN (
        'fan', 'press', 'radio', 'playlist', 'media_patronage', 'endorsement',
        'creator', 'promoter', 'venue', 'festival'
    ));
