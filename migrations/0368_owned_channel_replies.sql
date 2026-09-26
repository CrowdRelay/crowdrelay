-- The reply lane on the band's own channels: comments under its Instagram
-- and Facebook posts join the same queue as Reddit comments.
--
-- Most of the band's audience comments on Instagram and Facebook, on posts
-- the band owns, where nobody answered them. One table, not two: the
-- operator reads one queue, and the draft → review → route path is shared.
-- A row belongs either to a Reddit community post or to a synced social
-- post (content_sources, kind social_post) — never both.
--
--   platform            reddit | instagram | facebook
--   content_source_id   the synced post, for instagram/facebook rows
--   platform_comment_id renamed from reddit_comment_id: the platform's own
--                       comment id (t1_… on Reddit, a Graph id otherwise)

ALTER TABLE community_comments RENAME COLUMN reddit_comment_id TO platform_comment_id;

ALTER TABLE community_comments
    ADD COLUMN IF NOT EXISTS platform text NOT NULL DEFAULT 'reddit'
        CHECK (platform IN ('reddit', 'instagram', 'facebook')),
    ADD COLUMN IF NOT EXISTS content_source_id uuid
        REFERENCES content_sources(id) ON DELETE CASCADE,
    ALTER COLUMN community_post_id DROP NOT NULL;

ALTER TABLE community_comments
    DROP CONSTRAINT IF EXISTS community_comments_reddit_comment_id_check,
    DROP CONSTRAINT IF EXISTS community_comments_parent_id_check,
    DROP CONSTRAINT IF EXISTS community_comments_reply_comment_id_check,
    DROP CONSTRAINT IF EXISTS community_comments_workspace_id_reddit_comment_id_key;

ALTER TABLE community_comments
    ADD CONSTRAINT community_comments_parent_row_check CHECK (
        (platform = 'reddit' AND community_post_id IS NOT NULL AND content_source_id IS NULL)
        OR (platform <> 'reddit' AND content_source_id IS NOT NULL AND community_post_id IS NULL)
    ),
    ADD CONSTRAINT community_comments_platform_comment_id_check CHECK (
        (platform = 'reddit' AND platform_comment_id ~ '^t1_[A-Za-z0-9]{1,20}$')
        OR (platform <> 'reddit' AND platform_comment_id ~ '^[0-9_]{1,64}$')
    ),
    ADD CONSTRAINT community_comments_parent_id_check CHECK (
        (platform = 'reddit' AND parent_id ~ '^t[13]_[A-Za-z0-9]{1,20}$')
        OR (platform <> 'reddit' AND parent_id ~ '^[0-9_]{1,64}$')
    ),
    ADD CONSTRAINT community_comments_reply_comment_id_check CHECK (
        reply_comment_id IS NULL
        OR (platform = 'reddit' AND reply_comment_id ~ '^t1_[A-Za-z0-9]{1,20}$')
        OR (platform <> 'reddit' AND reply_comment_id ~ '^[0-9_]{1,64}$')
    ),
    ADD CONSTRAINT community_comments_platform_comment_key
        UNIQUE (workspace_id, platform, platform_comment_id);

CREATE INDEX IF NOT EXISTS community_comments_source_idx
    ON community_comments (workspace_id, content_source_id)
    WHERE content_source_id IS NOT NULL;
