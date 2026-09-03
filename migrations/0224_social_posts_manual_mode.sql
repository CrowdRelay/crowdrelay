-- Recreate the social_posts table (originally created in 0168, dropped in
-- 0178 when the old social executor was removed as dead code).
--
-- The new social_post_executor supports manual mode: posts are drafted and
-- marked `awaiting_manual_post` — the operator posts manually to the
-- platform and registers the post URL via the API.
--
-- Status lifecycle:
--   pending               → row created, not yet attempted
--   posting               → platform API call in progress (crash recovery reclaims)
--   posted                → platform accepted the submission, post URL recorded
--   failed                → unrecoverable failure (no connection, API rejection)
--   rate_limited          → platform returned 429; retried after rate_limited_until
--   awaiting_manual_post  → drafted, waiting for operator to post manually

CREATE TABLE IF NOT EXISTS social_posts (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id         UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id            UUID NOT NULL REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,
    -- The platform this post targets: 'facebook', 'instagram', 'x'.
    platform             TEXT NOT NULL,
    -- The post content (the full draft from the LLM output).
    content              JSONB NOT NULL,
    -- The smart link to include in the post (Signal signup URL).
    smart_link           TEXT,
    -- Platform-specific post identifier and URL after successful posting.
    platform_post_id     TEXT,
    platform_post_url    TEXT,
    status               TEXT NOT NULL DEFAULT 'pending'
                         CHECK (status IN (
                             'pending', 'posting', 'posted', 'failed',
                             'rate_limited', 'awaiting_manual_post'
                         )),
    error_message        TEXT,
    attempts             INT  NOT NULL DEFAULT 0,
    rate_limited_until   TIMESTAMPTZ,
    posted_at            TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (action_id)
);

-- Find unprocessed and retryable actions fast.
CREATE INDEX IF NOT EXISTS social_posts_status_idx
    ON social_posts (workspace_id, status, created_at);

-- Anti-spam check: has this platform been posted to recently?
CREATE INDEX IF NOT EXISTS social_posts_platform_idx
    ON social_posts (workspace_id, platform, posted_at DESC);

-- Platform cooldown query (works for pending/awaiting rows where posted_at is NULL).
CREATE INDEX IF NOT EXISTS social_posts_platform_created_idx
    ON social_posts (platform, created_at);
