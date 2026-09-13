-- Widen viryaos_growth_evidence.channel to include Telegram and Discord.
--
-- The original CHECK (migration 0150) only allows:
--   email, reddit_post, reddit_dm, signal_push, social_post, sms, other
--
-- ReachChannel gained TelegramPost/DiscordPost so evidence.channel can be
-- derived from the dispatching template's delivery surface — the same
-- vocabulary viryaos_reach_events already uses (migration 0225). Without
-- this widening, a telegram-poster or discord-poster dispatch would roll
-- back its entire decision transaction on a CHECK violation.

ALTER TABLE viryaos_growth_evidence
    DROP CONSTRAINT IF EXISTS viryaos_growth_evidence_channel_check;

ALTER TABLE viryaos_growth_evidence
    ADD CONSTRAINT viryaos_growth_evidence_channel_check
    CHECK (channel IN (
        'email', 'reddit_post', 'reddit_dm', 'signal_push',
        'social_post', 'sms', 'other',
        'telegram_post', 'discord_post'
    ));
