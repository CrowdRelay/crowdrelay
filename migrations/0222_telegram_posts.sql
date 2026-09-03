-- Telegram post execution ledger.
--
-- The brain dispatches the `telegram-poster` LLM worker which drafts channel
-- posts. The agent_outcomes worker maps the outcome to an
-- `agent.content.request` autopilot action. This table tracks the actual
-- Telegram Bot API delivery — the executor claims succeeded actions that have
-- no telegram_posts row yet, posts via sendMessage, and records the result.
--
-- Mirrors `community_posts` (Reddit) in structure and lifecycle:
--   pending → posting → posted (or failed / rate_limited / awaiting_manual_post)
--
-- `action_id` is UNIQUE — idempotency boundary so a crash-recovery re-run
-- cannot create a duplicate post row for the same autopilot action.

CREATE TABLE telegram_posts (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id         UUID NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id            UUID NOT NULL UNIQUE,
    channel              TEXT NOT NULL,
    message_id           BIGINT,
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

CREATE INDEX telegram_posts_workspace_status_idx
    ON telegram_posts (workspace_id, status);
CREATE INDEX telegram_posts_channel_created_idx
    ON telegram_posts (channel, created_at);
