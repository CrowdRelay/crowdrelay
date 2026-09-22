-- Peer-act home city and attributed facts (peer-act seed, plan §12-5).
--
-- place_peer_acts names a band that is not a tenant; until now it was a pure
-- identity row, which meant "which bands are based in this city" — the
-- support-slot question a researched band sheet exists to answer — had no
-- column to land on. Two additions mirror what 0308 did for rooms.
--
-- home_city_id is a resolved pointer, nullable by design: a band whose home
-- town nobody has stated keeps NULL rather than borrowing the city it was
-- last billed in. ON DELETE SET NULL matches the venue registry's city
-- posture — removing a catalogue city removes the pointer, not the act.
--
-- place_peer_act_facts is the attributed-claim table, the same shape as
-- place_venue_facts: an attribute, a value, a provenance class, the source
-- that made the claim, and a clock. workspace_id NULL is the whole
-- platform's knowledge — a band's home city, its public pages, its own
-- published words. Non-NULL is contributor-private: the contact address one
-- tenant's sheet carried is that tenant's lead, not shared knowledge —
-- contact is never global, for the same reason a room's booking address is
-- not. Provenance classes match place_peer_act_genres so one vocabulary
-- describes every claim about an act.
--
-- Dedupe follows the venue-facts rule: upsert on (peer, attribute,
-- provenance, source_ref) within a scope, as two partial unique indexes —
-- a NULL workspace_id never conflicts, so one tenant's private claim can
-- never overwrite another's. Nothing deletes on absence: a row missing from
-- the next sheet is a gap in the sheet, not a retraction.

ALTER TABLE place_peer_acts
    ADD COLUMN home_city_id uuid REFERENCES cities(id) ON DELETE SET NULL;
CREATE INDEX place_peer_acts_home_city_idx
    ON place_peer_acts (home_city_id) WHERE home_city_id IS NOT NULL;

CREATE TABLE place_peer_act_facts (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    peer_act_id  uuid NOT NULL REFERENCES place_peer_acts(id) ON DELETE CASCADE,
    attribute    text NOT NULL CHECK (btrim(attribute) <> '' AND char_length(attribute) <= 100),
    value        text NOT NULL CHECK (char_length(value) <= 2000),
    provenance   text NOT NULL CHECK (provenance IN
                     ('researched','event_evidence','open_directory','musicbrainz')),
    source_ref   text NOT NULL CHECK (btrim(source_ref) <> '' AND char_length(source_ref) <= 2000),
    observed_at  timestamptz NOT NULL,
    expires_at   timestamptz,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE
);

-- Global facts dedupe on the four-column key; private facts dedupe per
-- contributing workspace. An ON CONFLICT arbiter must name the same
-- predicate to find these.
CREATE UNIQUE INDEX place_peer_act_facts_global_uq
    ON place_peer_act_facts (peer_act_id, attribute, provenance, source_ref)
    WHERE workspace_id IS NULL;
CREATE UNIQUE INDEX place_peer_act_facts_private_uq
    ON place_peer_act_facts (peer_act_id, attribute, provenance, source_ref, workspace_id)
    WHERE workspace_id IS NOT NULL;

CREATE INDEX place_peer_act_facts_resolve_idx
    ON place_peer_act_facts (peer_act_id, attribute);
