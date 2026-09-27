-- Expression indexes for the normalized-subreddit joins.
--
-- The community-target ranking (`growth_intelligence/community_targets.rs`),
-- the audience-graph promotion and the vetting sweep all match a target to
-- its evidence with `normalize_subreddit(x) = normalize_subreddit(t.subreddit)`.
-- The plain `(workspace_id, community, ...)` index cannot serve that, so each
-- promoted community re-scanned every Reddit provenance row and every
-- community post: the ranking query took 475 ms on production data on
-- 2026-09-27 (42 targets x 2,365 provenance rows), 262 ms of it in the
-- provenance lateral and 208 ms in the durable-fans lateral, and it logged
-- as the API's slowest statement (up to 1.98 s). The cost grows with
-- targets times events.
--
-- `normalize_subreddit` is IMMUTABLE, which is what lets it be indexed.

CREATE INDEX IF NOT EXISTS fan_provenance_reddit_community_norm_idx
    ON fan_provenance_events (workspace_id, normalize_subreddit(community), event_kind, occurred_at)
    WHERE channel = 'reddit';

CREATE INDEX IF NOT EXISTS community_posts_posted_subreddit_norm_idx
    ON community_posts (workspace_id, normalize_subreddit(subreddit), posted_at)
    WHERE posted_at IS NOT NULL;
