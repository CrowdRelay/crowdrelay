-- Community posts ledger: tracks which autopilot community.engage.request
-- actions have been posted to Reddit (or failed). This is the executor's
-- processing table — analogous to agent_outcomes.status for the agent
-- outcome worker.
--
-- The executor polls viryaos_autopilot_actions for succeeded actions with
-- action_kind = 'community.engage.request' that don't have a community_posts
-- row yet, posts to Reddit via OAuth, and records the result here.
--
-- UNIQUE(action_id) makes the insert idempotent — a re-run after a crash
-- between the INSERT and the Reddit API call will find the pending row and
-- retry it rather than creating a duplicate.
--
-- Status lifecycle:
--   pending     → row created, not yet attempted
--   posting     → Reddit API call in progress (crash recovery reclaims these)
--   posted      → Reddit accepted the submission, post_id + url recorded
--   failed      → unrecoverable failure (no Reddit connection, API rejection)
--   rate_limited → Reddit returned 429; retried after rate_limited_until

CREATE TABLE IF NOT EXISTS community_posts (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id        UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id           UUID NOT NULL REFERENCES viryaos_autopilot_actions(id) ON DELETE CASCADE,
    -- target_id is the viryaos_outreach_targets.id the post was drafted for.
    -- It's a logical reference — the target may be deleted independently, so
    -- no FK constraint is enforced.
    target_id           UUID,
    subreddit           TEXT NOT NULL,
    title               TEXT NOT NULL,
    body                TEXT NOT NULL,
    smart_link          TEXT,
    reddit_post_id      TEXT,
    reddit_post_url     TEXT,
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
CREATE INDEX IF NOT EXISTS community_posts_status_idx
    ON community_posts (workspace_id, status, created_at);

-- Anti-spam check: has this subreddit been posted to recently?
CREATE INDEX IF NOT EXISTS community_posts_subreddit_idx
    ON community_posts (workspace_id, subreddit, posted_at DESC);
