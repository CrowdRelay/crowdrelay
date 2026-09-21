-- Community reposts: the language a target community writes in, and the
-- media a relayed band post carries.
--
-- `agent_outreach_targets.language` — the community's posting language
-- (`pl`, `en`, ...). The target-discovery outcome declares it (the agent
-- scraped the subreddit and knows); NULL means nobody recorded it and the
-- repost drafter infers the language from the subreddit's own description.
-- Precedent: `viryaos_community_outreach_targets.language` carried the same
-- idea on the older outreach table this one superseded.
--
-- `community_posts` media columns — the social-post relay used to carry a
-- caption and nothing else, so forum reposts went out as bare text. The
-- repost pipeline now attaches the post's original picture:
--   image_url  — a fetchable image URL at claim time (Graph CDN; may be
--                re-minted from media_id when it has expired)
--   media_id   — the Graph object the image belongs to; `/{id}?fields=media_url`
--                (or `thumbnail_url` for a video still) re-mints the URL
--   source_url — the band's own permalink, used for the link-post fallback
--                and as the attribution target
--   post_kind  — what actually shipped: 'image' | 'link' | 'self'. Written
--                by the executor from the agents service response so the
--                ledger says what went out, not what was attempted.

ALTER TABLE agent_outreach_targets
    ADD COLUMN IF NOT EXISTS language TEXT
    CHECK (language IS NULL OR char_length(language) <= 8);

ALTER TABLE community_posts
    ADD COLUMN IF NOT EXISTS image_url TEXT,
    ADD COLUMN IF NOT EXISTS media_id TEXT,
    ADD COLUMN IF NOT EXISTS source_url TEXT,
    ADD COLUMN IF NOT EXISTS post_kind TEXT
        CHECK (post_kind IS NULL OR post_kind = ANY (ARRAY['image', 'link', 'self']));
