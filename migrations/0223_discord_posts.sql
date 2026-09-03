-- Discord post execution ledger + connection extension.
--
-- The brain dispatches the `discord-poster` LLM worker which drafts Discord
-- messages. This table tracks the actual Discord Bot API delivery — the
-- executor claims succeeded actions that have no discord_posts row yet,
-- posts via the Bot API, and records the result.
--
-- Mirrors `community_posts` (Reddit) in structure and lifecycle:
--   pending → posting → posted (or failed / rate_limited / awaiting_manual_post)
--
-- `action_id` is UNIQUE — idempotency boundary so a crash-recovery re-run
-- cannot create a duplicate post row for the same autopilot action.

CREATE TABLE discord_posts (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id         UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id            UUID NOT NULL UNIQUE,
    channel_id           TEXT NOT NULL,
    message_id           TEXT,
    status               TEXT NOT NULL DEFAULT 'pending'
                         CHECK (status IN (
                             'pending', 'posting', 'posted', 'failed',
                             'rate_limited', 'awaiting_manual_post'
                         )),
    posted_at            TIMESTAMPTZ,
    error_message        TEXT,
    rate_limited_until   TIMESTAMPTZ,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX discord_posts_workspace_status_idx
    ON discord_posts (workspace_id, status);
CREATE INDEX discord_posts_channel_created_idx
    ON discord_posts (channel_id, created_at);
