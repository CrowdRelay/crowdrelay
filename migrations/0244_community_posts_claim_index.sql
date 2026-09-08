-- Composite index for the community executor's claim transaction.
--
-- The claim query filters on (workspace_id, status) and the cooldown
-- subquery filters on (workspace_id, subreddit, status, posted_at) to
-- exclude subreddits with a recent successful post. Without a composite
-- index the NOT EXISTS subquery scans community_posts for each candidate
-- row, which grows with the post history.
--
-- The existing single-column indexes (community_posts_status_idx on
-- status, community_posts_subreddit_idx on subreddit) do not support
-- this combined predicate efficiently.
CREATE INDEX IF NOT EXISTS community_posts_cooldown_idx
    ON community_posts (workspace_id, subreddit, status, posted_at);
