-- Add `awaiting_manual_post` status for the manual posting flow.
-- When Reddit API access is unavailable, the community executor creates
-- rows with this status instead of posting automatically. The operator
-- posts manually on Reddit, then registers the post URL via the API,
-- which transitions the row to `posted` so the metrics poller can track it.
ALTER TABLE community_posts
    DROP CONSTRAINT IF EXISTS community_posts_status_check,
    ADD CONSTRAINT community_posts_status_check
    CHECK (status = ANY (ARRAY['pending'::text, 'posting'::text, 'posted'::text, 'failed'::text, 'rate_limited'::text, 'awaiting_manual_post'::text]));
