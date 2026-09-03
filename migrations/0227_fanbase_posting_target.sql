-- Where a post goes, as distinct from which audience is being measured.
--
-- `fanbase_connections.provider_account_id` means whatever the platform's
-- read API needs: a subreddit name for reddit, a channel for telegram, and —
-- for discord — an invite code, because member counts come from
-- `GET /api/v9/invites/{code}?with_counts=true`.
--
-- The discord executor read the same column as the channel to post to:
-- `POST /channels/{channel_id}/messages`. Production holds
-- `provider_account_id = 'BBdDV6gVy'`, an invite code. A channel id is a
-- numeric snowflake. So metrics worked, posting could not, and the two
-- readers disagreed about what one column meant. Nothing failed loudly
-- because no discord post has ever been attempted.
--
-- One column cannot hold both. `posting_target_ref` is the address a write
-- goes to; `provider_account_id` stays the identifier a read is about. Where
-- they are the same thing — telegram posts to the channel it measures — the
-- new column stays NULL and the executor falls back, so nothing that works
-- today changes.

ALTER TABLE fanbase_connections
    ADD COLUMN IF NOT EXISTS posting_target_ref text
        CHECK (posting_target_ref IS NULL OR (
            btrim(posting_target_ref) <> '' AND char_length(posting_target_ref) <= 200));

COMMENT ON COLUMN fanbase_connections.posting_target_ref IS
    'The address a post is written to, when it differs from the account being measured. Discord needs it: provider_account_id holds the invite code the member-count API reads, and this holds the channel snowflake the message API writes to. NULL means the two are the same and provider_account_id serves both.';

COMMENT ON COLUMN fanbase_connections.provider_account_id IS
    'The identifier the platform read API is about — subreddit name, telegram channel, discord invite code. For writing, see posting_target_ref.';
