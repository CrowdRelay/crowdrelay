-- Attributed venue facts (Sprint 4V.2, plan §12-2). `place_venues` stays a
-- pure identity row — one room per (city_id, name_key) and nothing else.
-- Every claim about a room is a fact here: an attribute, a value, one of
-- five provenance classes, the source that made the claim, and the clock
-- that says when it was seen. Provenance is a trust order, not a label —
-- resolution walks 'played' → 'researched' → 'event_evidence' →
-- 'open_directory' → 'commercial_directory' and takes the first
-- non-expired hit, so a room the tenant played outranks a directory's
-- guess.
--
-- A licensed fact carries expires_at as a deletion deadline. Expiry is
-- filtered at read time — now() is not immutable, so the resolve index is
-- a plain composite, not the partial index a WHERE expires_at clause would
-- want.
--
-- workspace_id NULL means the fact is global: capacity, genres, the room's
-- own published terms are the same for every tenant. Non-NULL means
-- contributor-private: contact and money are never global, and one
-- tenant's fit judgement is exactly what a competitor would like to read.
-- The global read (audience city_venues) filters on workspace_id IS NULL,
-- so a private fact can never leak into a cross-tenant answer.
--
-- Facts upsert on (venue_id, attribute, provenance, source_ref) *within a
-- scope*: re-researching the same source refreshes the claim rather than
-- stacking a second one. The scope is part of the key — two partial unique
-- indexes rather than one constraint, because NULL workspace_id never
-- conflicts and a single (…, source_ref) key would let one tenant's
-- private claim overwrite another's. Nothing here deletes on absence — a
-- researched fact lives until it is re-researched or expires, because a
-- row missing from the next sheet is a gap in the sheet, not a retraction
-- of the claim.

CREATE TABLE place_venue_facts (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    venue_id     uuid NOT NULL REFERENCES place_venues(id) ON DELETE CASCADE,
    attribute    text NOT NULL CHECK (btrim(attribute) <> '' AND char_length(attribute) <= 100),
    value        text NOT NULL CHECK (char_length(value) <= 2000),
    provenance   text NOT NULL CHECK (provenance IN
                     ('played','researched','event_evidence','open_directory','commercial_directory')),
    source_ref   text NOT NULL CHECK (btrim(source_ref) <> '' AND char_length(source_ref) <= 2000),
    observed_at  timestamptz NOT NULL,
    expires_at   timestamptz,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE
);

-- Global facts dedupe on the four-column key; private facts dedupe per
-- contributing workspace. An ON CONFLICT arbiter must name the same
-- predicate to find these.
CREATE UNIQUE INDEX place_venue_facts_global_uq
    ON place_venue_facts (venue_id, attribute, provenance, source_ref)
    WHERE workspace_id IS NULL;
CREATE UNIQUE INDEX place_venue_facts_private_uq
    ON place_venue_facts (venue_id, attribute, provenance, source_ref, workspace_id)
    WHERE workspace_id IS NOT NULL;

CREATE INDEX place_venue_facts_resolve_idx
    ON place_venue_facts (venue_id, attribute);
