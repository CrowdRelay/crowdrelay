-- Extend the existing social_posts table (migration 0168) to support
-- manual posting mode and platform filtering.
--
-- The original table was designed for auto-posting via platform Graph APIs
-- (Facebook, Instagram, X). The social_post_executor now also supports
-- manual mode where posts are drafted and marked `awaiting_manual_post`
-- — the operator posts manually and records the result.
--
-- This migration:
-- 1. Adds `awaiting_manual_post` to the status CHECK constraint.
-- 2. Adds an index on (platform, created_at) for the platform cooldown
--    query (the existing platform_idx uses posted_at which is NULL for
--    pending/awaiting rows).

ALTER TABLE social_posts
    DROP CONSTRAINT IF EXISTS social_posts_status_check;

ALTER TABLE social_posts
    ADD CONSTRAINT social_posts_status_check CHECK (status IN (
        'pending', 'posting', 'posted', 'failed', 'rate_limited',
        'awaiting_manual_post'
    ));

CREATE INDEX IF NOT EXISTS social_posts_platform_created_idx
    ON social_posts (platform, created_at);
