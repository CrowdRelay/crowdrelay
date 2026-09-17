-- When this address last wrote to the band's mailbox (message `From` = the
-- address, `internalDate` of the message). `last_seen_at` is any sighting in
-- any header; this is direction. Set by the Gmail sync only, monotonic.
ALTER TABLE viryaos_drive_contacts
    ADD COLUMN IF NOT EXISTS last_inbound_at timestamptz;
