-- Close the attribution gap between the channels that send and the ledger
-- the brain learns from.
--
-- Two facts made the outbound loop deaf. `record_community_conversion` gated
-- attribution on `channel_community IS NOT NULL`, and only the Reddit path
-- ever set it — the social executor's links carried `channel_source` alone,
-- so a signup from an Instagram post was recorded and never attributed. And
-- Telegram/Discord posts went out with no tracked link at all: 0337 declined
-- to add `smart_link` columns there because no writer existed. This migration
-- adds the columns because the writers now exist — the executors bind a
-- tenant-origin smart link to every post, so a signup from a Telegram channel
-- or a Discord server attributes the same way a Reddit community does.
--
-- `fan_provenance_events.anonymous_visitor_id` is the click half of the same
-- loop. `fan_id` was always nullable — exposure and interaction rows may be
-- written before the fan exists — but nothing carried the visitor's identity,
-- so a click could never be linked to the fan it later became. The column
-- lets the click batch write `interaction` rows anonymously and lets the
-- signup path link the whole history once the fan is known.
--
-- Idempotent: every statement is additive and guarded.

ALTER TABLE telegram_posts
    ADD COLUMN IF NOT EXISTS smart_link text;

ALTER TABLE telegram_posts
    ADD COLUMN IF NOT EXISTS smart_link_id uuid;

-- Composite FK, same convention as social_posts: the link and the post
-- belong to the same tenant or the join does not exist.
ALTER TABLE telegram_posts
    DROP CONSTRAINT IF EXISTS telegram_posts_smart_link_fk;
ALTER TABLE telegram_posts
    ADD CONSTRAINT telegram_posts_smart_link_fk
    FOREIGN KEY (workspace_id, smart_link_id)
    REFERENCES smart_links (workspace_id, id)
    ON DELETE RESTRICT;

ALTER TABLE discord_posts
    ADD COLUMN IF NOT EXISTS smart_link text;

ALTER TABLE discord_posts
    ADD COLUMN IF NOT EXISTS smart_link_id uuid;

ALTER TABLE discord_posts
    DROP CONSTRAINT IF EXISTS discord_posts_smart_link_fk;
ALTER TABLE discord_posts
    ADD CONSTRAINT discord_posts_smart_link_fk
    FOREIGN KEY (workspace_id, smart_link_id)
    REFERENCES smart_links (workspace_id, id)
    ON DELETE RESTRICT;

ALTER TABLE fan_provenance_events
    ADD COLUMN IF NOT EXISTS anonymous_visitor_id uuid;

-- The signup-time link is a point update by visitor id; the click-side read
-- aggregates interactions by channel, so the event-kind index already covers
-- it and no second index is owed.
CREATE INDEX IF NOT EXISTS idx_fan_provenance_visitor
    ON fan_provenance_events (workspace_id, anonymous_visitor_id)
    WHERE anonymous_visitor_id IS NOT NULL;
