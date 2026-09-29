-- community_posts grew up Reddit-shaped, but a community delivery is not a
-- Reddit delivery: the audience graph already holds Discord servers, forums,
-- Telegram groups and Lemmy communities, and the manual lane has to carry a
-- draft for any of them. Until now the ledger could not even name the
-- platform a row belongs to — `subreddit` was the whole address.
--
--   platform   the community surface this delivery targets. `reddit` is the
--              default because every existing row is one. The community
--              executor claims `platform = 'reddit'` rows only; a non-Reddit
--              delivery seeds as `awaiting_manual_post` and waits for a
--              person (or a platform executor) rather than sitting claimable
--              in a lane that cannot send it.
--   place_id   the discovery_places row the delivery targets, when the
--              outreach target carries one. Logical reference, no FK — the
--              place may be retired independently and the posting record
--              must survive it. This is what lets `discovery_place_rules`
--              gates (flair-required, approval-required communities) hold a
--              draft at seed time instead of at the next moderator removal.

ALTER TABLE community_posts
    ADD COLUMN IF NOT EXISTS platform text NOT NULL DEFAULT 'reddit'
        CHECK (platform = ANY (ARRAY[
            'reddit', 'discord', 'telegram', 'forum', 'lemmy', 'brutalland', 'other'
        ])),
    ADD COLUMN IF NOT EXISTS place_id uuid;

-- Existing rows get their place from the outreach target they were drafted
-- for, when that target knows it.
UPDATE community_posts AS post
SET place_id = target.place_id
FROM agent_outreach_targets AS target
WHERE post.place_id IS NULL
  AND target.id = post.target_id
  AND target.place_id IS NOT NULL;

-- The claim lane and every per-place query reads platform; the veto and the
-- rules gate join through place_id.
CREATE INDEX IF NOT EXISTS community_posts_platform_status_idx
    ON community_posts (workspace_id, platform, status, created_at);

CREATE INDEX IF NOT EXISTS community_posts_place_idx
    ON community_posts (workspace_id, place_id)
    WHERE place_id IS NOT NULL;

-- A community delivery that is not a Reddit, Telegram or Discord channel
-- post still reaches an audience the credit allocator must count. `other`
-- was the honest label; `community_post` is the same fact with the name of
-- the lane on it. Additive only — this check may never lose a value.
ALTER TABLE reach_events
    DROP CONSTRAINT IF EXISTS reach_events_channel_check;

ALTER TABLE reach_events
    ADD CONSTRAINT reach_events_channel_check
    CHECK (channel = ANY (ARRAY[
        'email'::text, 'reddit_post'::text, 'reddit_dm'::text,
        'signal_push'::text, 'social_post'::text, 'sms'::text, 'other'::text,
        'telegram_post'::text, 'discord_post'::text, 'community_post'::text
    ]));
