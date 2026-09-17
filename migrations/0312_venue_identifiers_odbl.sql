-- The resolution anchor (§12-4): (scheme, identifier) is the PK — one OSM
-- node, one Wikidata Qid, one MusicBrainz id is one room, whoever saw it.
CREATE TABLE place_venue_identifiers (
    scheme     text NOT NULL CHECK (scheme IN
                 ('osm_node','osm_way','wikidata','musicbrainz','google_place_id','ticketmaster')),
    identifier text NOT NULL CHECK (btrim(identifier) <> '' AND char_length(identifier) <= 200),
    venue_id   uuid NOT NULL REFERENCES place_venues(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (scheme, identifier)
);
CREATE INDEX place_venue_identifiers_venue_idx
    ON place_venue_identifiers (venue_id);

-- OSM gives a location on every node — the venue row gains one, the
-- cities-table rule (both or neither) applies.
ALTER TABLE place_venues
    ADD COLUMN latitude double precision,
    ADD COLUMN longitude double precision,
    ADD CHECK ((latitude IS NULL AND longitude IS NULL)
               OR (latitude IS NOT NULL AND longitude IS NOT NULL));

-- ODbL is share-alike + attribution-required: the licence travels with the
-- fact, in the first commit, not retrofitted.
ALTER TABLE place_venue_facts
    ADD COLUMN licence text CHECK (licence IS NULL OR licence = 'odbl');
