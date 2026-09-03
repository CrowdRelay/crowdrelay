-- Widen viryaos_reach_events CHECK constraints to include Telegram and
-- Discord channels.
--
-- The original CHECK (migration 0146) only allowed:
--   recipient_kind: fan, outreach_target, subreddit_audience,
--                   platform_audience, community
--   channel: email, reddit_post, reddit_dm, signal_push,
--            social_post, sms, other
--
-- The telegram_executor and discord_executor insert:
--   recipient_kind: 'telegram_channel', 'discord_channel'
--   channel: 'telegram_post', 'discord_post'
--
-- Without this migration, any successful Telegram/Discord post will hit a
-- CHECK violation when the reach ledger is written.

ALTER TABLE viryaos_reach_events
    DROP CONSTRAINT IF EXISTS viryaos_reach_events_recipient_kind_check;

ALTER TABLE viryaos_reach_events
    ADD CONSTRAINT viryaos_reach_events_recipient_kind_check
    CHECK (recipient_kind IN (
        'fan', 'outreach_target', 'subreddit_audience', 'platform_audience',
        'community', 'telegram_channel', 'discord_channel'
    ));

ALTER TABLE viryaos_reach_events
    DROP CONSTRAINT IF EXISTS viryaos_reach_events_channel_check;

ALTER TABLE viryaos_reach_events
    ADD CONSTRAINT viryaos_reach_events_channel_check
    CHECK (channel IN (
        'email', 'reddit_post', 'reddit_dm', 'signal_push',
        'social_post', 'sms', 'other',
        'telegram_post', 'discord_post'
    ));
