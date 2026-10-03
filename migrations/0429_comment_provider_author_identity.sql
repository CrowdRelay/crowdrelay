-- Stable provider identity for people observed in comment harvests.
--
-- Display names and handles are not durable person identity:
-- * Instagram usernames can change;
-- * Facebook display names are not unique;
-- * YouTube display names can change.
--
-- The provider responses already expose stable author/channel ids. Persist them
-- beside the comment receipt so FAN SCOUT can bind observations to the same
-- real person across renames. NULL is allowed for providers (such as Reddit's
-- current adapter) whose contract does not expose a separate stable id.

ALTER TABLE community_comments
    ADD COLUMN IF NOT EXISTS provider_author_id text
        CHECK (
            provider_author_id IS NULL
            OR (
                btrim(provider_author_id) <> ''
                AND char_length(provider_author_id) <= 256
            )
        );

COMMENT ON COLUMN community_comments.provider_author_id IS
    'Stable provider user/channel id returned with this comment. FAN SCOUT prefers it over a mutable/non-unique display handle.';

CREATE INDEX IF NOT EXISTS community_comments_provider_author_idx
    ON community_comments (workspace_id, platform, provider_author_id)
    WHERE provider_author_id IS NOT NULL;
