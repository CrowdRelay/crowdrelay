-- Removal state on published community posts.
--
-- Reddit tells a post's author when a moderator, AutoModerator or Reddit's own
-- site-wide filters removed it (`removed_by_category`). Until now the metrics
-- read kept score, upvotes, comments and ratio and dropped that field, so a
-- removed post — or an account whose posts were being silently filtered —
-- looked exactly like a quiet one, and the executor kept posting.
--
-- These columns are what `crowdrelay_domain::reddit_standing` reads to decide
-- whether unattended posting may continue and how often:
--
--   removed_by_category  Reddit's category, verbatim, the first time a read
--                        saw the post removed. NULL: never seen removed.
--   removal_seen_at      When that read happened.
--   last_seen_live_at    The latest removal-aware read that saw the post
--                        live. NULL: no read could establish removal state,
--                        and the post counts as survived for nothing.

ALTER TABLE community_posts
    ADD COLUMN IF NOT EXISTS removed_by_category text
        CHECK (removed_by_category IS NULL OR (
            btrim(removed_by_category) <> '' AND char_length(removed_by_category) <= 64)),
    ADD COLUMN IF NOT EXISTS removal_seen_at timestamptz,
    ADD COLUMN IF NOT EXISTS last_seen_live_at timestamptz;

-- The standing reads a workspace's recent published posts on every claim.
CREATE INDEX IF NOT EXISTS community_posts_published_history_idx
    ON community_posts (workspace_id, posted_at DESC)
    WHERE status = 'posted';
