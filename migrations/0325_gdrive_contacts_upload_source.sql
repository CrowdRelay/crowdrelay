-- P.2: the operator's own spreadsheet is an intake source too.
--
-- `viryaos_drive_contacts.sources` documented "which intake sources have
-- seen this address" and CHECK-bound the answer to the two connectors.
-- A pasted or uploaded sheet is a third sighting with the same semantics —
-- one row per address, `sources` naming every way it arrived. Widening the
-- constraint is additive only: existing rows already satisfy it.

ALTER TABLE viryaos_drive_contacts
    DROP CONSTRAINT viryaos_drive_contacts_sources_check;

ALTER TABLE viryaos_drive_contacts
    ADD CONSTRAINT viryaos_drive_contacts_sources_check
    CHECK (array_length(sources, 1) >= 1
           AND sources <@ ARRAY['gdrive', 'gmail', 'upload']::text[]);
