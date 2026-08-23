-- Which channel actually brought somebody.
--
-- `fan_acquisition_events.source` looks like it answers this and does not: it
-- carries the *consent* source, meaning where permission was recorded rather
-- than where the person came from. So today there is no channel attribution at
-- all, and a campaign run across ten communities cannot say which one worked.
--
-- The join key already exists. A click on `/v1/go/{slug}` writes
-- `click_events(smart_link_id, anonymous_visitor_id)`, and a signup writes
-- `fan_acquisition_events(anonymous_visitor_id)`. What was missing is channel
-- identity on the link itself, which is added here.
--
-- The link is the channel. One link per (source, community, creative) is
-- exactly how a person actually posts — a different link for r/Metal than for a
-- Facebook group than for the second image — so nothing extra has to be
-- remembered at signup time and the API contract does not change.

ALTER TABLE smart_links
    -- Broad channel: 'reddit', 'facebook', 'discord', 'linkedin', 'youtube',
    -- 'instagram', 'venue', 'band'. Deliberately free text and not a CHECK:
    -- inventing a channel is a Tuesday afternoon decision, and a constraint
    -- here would mean a migration every time somebody tries a new place.
    ADD COLUMN channel_source text
        CHECK (channel_source IS NULL OR (btrim(channel_source) <> '' AND char_length(channel_source) <= 64));

ALTER TABLE smart_links
    -- The specific place inside that channel: which subreddit, which group,
    -- which server. This is the field that answers "which community converts",
    -- which is the question a zero-budget campaign lives or dies on.
    ADD COLUMN channel_community text
        CHECK (channel_community IS NULL OR (btrim(channel_community) <> '' AND char_length(channel_community) <= 120));

ALTER TABLE smart_links
    -- Which post, image or wording. Two links to the same community with
    -- different creatives is how a test gets run without a testing framework.
    ADD COLUMN channel_creative text
        CHECK (channel_creative IS NULL OR (btrim(channel_creative) <> '' AND char_length(channel_creative) <= 120));

-- Attribution walks backwards from a signup: find that visitor's most recent
-- click at or before the signup. Without this index that is a scan per fan.
CREATE INDEX IF NOT EXISTS click_events_visitor_time_idx
    ON click_events (workspace_id, anonymous_visitor_id, occurred_at DESC)
    WHERE anonymous_visitor_id IS NOT NULL;

-- Reporting groups by channel, so the link lookup behind each click is hot too.
CREATE INDEX IF NOT EXISTS smart_links_channel_idx
    ON smart_links (workspace_id, channel_source, channel_community)
    WHERE channel_source IS NOT NULL;
