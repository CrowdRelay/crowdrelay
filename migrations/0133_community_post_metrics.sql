-- Community post performance metrics: time-series tracking of Reddit post
-- engagement (upvotes, comments, score). The community executor polls
-- Reddit's API periodically for posts created in the last 72h and records
-- a snapshot here.
--
-- This is the "eyes" of the growth loop — the system learns which posts
-- work. The brain can query this table directly to inform future dispatch
-- decisions (e.g. "this subreddit gets 0 engagement, stop posting there").
--
-- One row per poll cycle per post (time series, not latest-only). This
-- lets us track engagement velocity over time, not just a final snapshot.

CREATE TABLE IF NOT EXISTS community_post_metrics (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id    UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    community_post_id UUID NOT NULL REFERENCES community_posts(id) ON DELETE CASCADE,
    reddit_post_id  TEXT NOT NULL,
    measured_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    score           INT  NOT NULL DEFAULT 0,
    upvotes         INT  NOT NULL DEFAULT 0 CHECK (upvotes >= 0),
    num_comments    INT  NOT NULL DEFAULT 0 CHECK (num_comments >= 0),
    upvote_ratio    DOUBLE PRECISION,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, community_post_id, measured_at)
);

-- Find all metrics for a post, ordered by time.
CREATE INDEX IF NOT EXISTS community_post_metrics_post_idx
    ON community_post_metrics (workspace_id, community_post_id, measured_at DESC);

-- Add a column to community_posts to track when we last polled metrics,
-- so the poller doesn't re-fetch posts that were just measured.
ALTER TABLE community_posts
    ADD COLUMN IF NOT EXISTS metrics_last_fetched_at TIMESTAMPTZ;
