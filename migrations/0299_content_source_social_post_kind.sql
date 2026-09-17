-- Extend the trusted-content vocabulary with `social_post`.
--
-- `social_post` is a post the band published on an owned social account —
-- synced by the social-post watcher from the connected Facebook Page or
-- Instagram Business profile. It is a fact row (title, link, timestamp, the
-- band's own caption as body), not a draft: the amplification path relays it
-- rather than inventing a post about it.
ALTER TABLE viryaos_content_sources
    DROP CONSTRAINT IF EXISTS viryaos_content_sources_source_kind_check;
ALTER TABLE viryaos_content_sources
    ADD CONSTRAINT viryaos_content_sources_source_kind_check
    CHECK (source_kind IN ('event', 'release', 'show_completed', 'video', 'story', 'social_post'));
