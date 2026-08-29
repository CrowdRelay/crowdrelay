-- Social posts ledger: tracks which autopilot social.post.request actions
-- have been posted to external social platforms (Facebook, Instagram, X/Twitter).
-- This is the social executor's processing table — analogous to community_posts
-- for Reddit community engagement.
--
-- The executor polls viryaos_autopilot_actions for succeeded actions with
-- action_kind = 'social.post.request' that don't have a social_posts row yet,
-- loads the workspace's platform OAuth token from fanbase_connections, and
-- submits the post via the platform's Graph API.
--
-- UNIQUE(action_id) makes the insert idempotent — a re-run after a crash
-- between the INSERT and the platform API call will find the pending row and
-- retry it rather than creating a duplicate.
--
-- Status lifecycle:
--   pending      → row created, not yet attempted
--   posting      → platform API call in progress (crash recovery reclaims these)
--   posted       → platform accepted the submission, post_id + url recorded
--   failed       → unrecoverable failure (no connection, API rejection)
--   rate_limited → platform returned 429; retried after rate_limited_until

CREATE TABLE IF NOT EXISTS social_posts (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id           UUID NOT NULL REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,
    -- The platform this post targets: 'facebook', 'instagram', 'x_twitter'.
    platform            TEXT NOT NULL,
    -- The post content. For Facebook: message + link. For Instagram: caption +
    -- media_url. For X/Twitter: text (≤280 chars).
    content             JSONB NOT NULL,
    -- The smart link to include in the post (Signal signup URL).
    smart_link          TEXT,
    -- Platform-specific post identifier and URL after successful posting.
    platform_post_id    TEXT,
    platform_post_url   TEXT,
    status              TEXT NOT NULL DEFAULT 'pending'
                        CHECK (status IN ('pending', 'posting', 'posted', 'failed', 'rate_limited')),
    error_message       TEXT,
    attempts            INT  NOT NULL DEFAULT 0,
    rate_limited_until  TIMESTAMPTZ,
    posted_at           TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (action_id)
);

-- Find unprocessed and retryable actions fast.
CREATE INDEX IF NOT EXISTS social_posts_status_idx
    ON social_posts (workspace_id, status, created_at);

-- Anti-spam check: has this platform been posted to recently?
CREATE INDEX IF NOT EXISTS social_posts_platform_idx
    ON social_posts (workspace_id, platform, posted_at DESC);
