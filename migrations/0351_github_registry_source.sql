-- The shared registry workbook lives in a GitHub repo and arrives through
-- the github_registry_sync worker. A sheet committed there is a fourth
-- sighting with the same semantics as the connectors: one row per address,
-- `sources` naming every way it arrived — and, like a Drive file, a
-- re-listed file is the truth about its own rows, so 'github' marks
-- disappeared and owns the source_file anchor the same way 'gdrive' does.
-- Widening the constraint is additive only: existing rows already satisfy it.

ALTER TABLE drive_contacts
    DROP CONSTRAINT drive_contacts_sources_check;

ALTER TABLE drive_contacts
    ADD CONSTRAINT drive_contacts_sources_check
    CHECK (array_length(sources, 1) >= 1
           AND sources <@ ARRAY['gdrive', 'gmail', 'upload', 'github']::text[]);
