-- Format→fan attribution edge (§4b-4 rule 6).
--
-- The conversion chain already joins a fan back to the community post they
-- clicked (`fan_provenance_events.action_id` → the posting action →
-- `payload.source_id` → `viryaos_content_sources`), and the outcome-ingest
-- gate guarantees every posted thread names a live `video` source. What it
-- could not answer was §4b-4's third concentration question — which *content
-- format* converts — because the produced artifact carried no link back to
-- the catalogue entry it was made in.
--
-- `viryaos_content_sources.format_key` is the declaration side: whoever
-- files the video names the catalogue format it is (nullable — a source
-- filed without one is honest, not zero). The FK keeps the column to real
-- catalogue keys, the same discipline `content_suggestions.format_key`
-- already holds.
--
-- `fan_provenance_events.format_key` is the stamp side: the conversion row
-- copies the promoted source's declared format at write time — plain TEXT,
-- no FK, the same event-facts treatment `community` and `source_target`
-- already get. A conversion that never touched a format-declared source
-- stays NULL and reports "unrecorded", never a guessed bucket.
ALTER TABLE viryaos_content_sources
    ADD COLUMN format_key TEXT
        REFERENCES viryaos_content_format_entries(key);

ALTER TABLE fan_provenance_events
    ADD COLUMN format_key TEXT;
