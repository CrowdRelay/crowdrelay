-- Extend the trusted-content vocabulary with `video` and `story`.
--
-- `video` is a published video fact (the YouTube watcher writes one row per
-- video id); `story` is a first-person account the tenant entered through the
-- control plane. Both exist so generated copy has real material to ground in:
-- the engager may share a video or narrate a story, but may not invent one.
ALTER TABLE viryaos_content_sources
    DROP CONSTRAINT IF EXISTS viryaos_content_sources_source_kind_check;
ALTER TABLE viryaos_content_sources
    ADD CONSTRAINT viryaos_content_sources_source_kind_check
    CHECK (source_kind IN ('event', 'release', 'show_completed', 'video', 'story'));
