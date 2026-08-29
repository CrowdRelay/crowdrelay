-- Reach events — unified ledger of every outbound contact attempt.
--
-- The brain needs to know: who did we contact, how, what happened, and did
-- they convert to a real fan? This table unifies email sends, Reddit posts,
-- Signal pushes, and any future channel into a single normalized ledger so
-- the brain can learn reach-to-fan conversion rates per channel, per
-- template, per audience.
--
-- This is NOT a replacement for the channel-specific tables
-- (community_posts, fan_push_deliveries, viryaos_outreach_interactions).
-- Those remain the source of truth for channel-specific details. This table
-- is a normalized projection that the brain reads to compute reach metrics
-- and feed the calibration loop.
--
-- Design:
-- - `recipient_kind` distinguishes individual contacts (fan, outreach target)
--   from broadcast audiences (subreddit, platform). For broadcasts,
--   `estimated_reach` is the group size (e.g. subreddit subscribers).
-- - `status` is a state machine: sent → delivered → (opened | clicked) →
--   (replied | converted | bounced | complained | ignored).
-- - `action_id` links back to the autopilot action that triggered the reach.
-- - `converted_fan_id` is set when the recipient becomes a fan, closing the
--   reach-to-fan attribution loop.

CREATE TABLE IF NOT EXISTS viryaos_reach_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,

    -- The autopilot action that triggered this reach event (optional —
    -- manual reaches may not have one).
    action_id uuid REFERENCES viryaos_autopilot_actions(id) ON DELETE SET NULL,

    -- Who was reached: either an individual (fan or outreach target) or a
    -- broadcast audience.
    recipient_kind text NOT NULL CHECK (recipient_kind IN ('fan', 'outreach_target', 'subreddit_audience', 'platform_audience', 'community')),
    recipient_id text NOT NULL CHECK (btrim(recipient_id) <> ''),

    -- The channel used to reach the recipient.
    channel text NOT NULL CHECK (channel IN ('email', 'reddit_post', 'reddit_dm', 'signal_push', 'social_post', 'sms', 'other')),

    -- The worker template that produced this reach (e.g. 'community-engager',
    -- 'signal-inviter', 'reddit-scanner').
    template_id text NOT NULL CHECK (btrim(template_id) <> '' AND char_length(template_id) <= 64),

    -- The estimated size of the audience reached. For individual contacts
    -- (fan, outreach_target), this is 1. For broadcasts (subreddit_post),
    -- this is the estimated subreddit subscriber count or post viewership.
    estimated_reach integer NOT NULL DEFAULT 1 CHECK (estimated_reach >= 1),

    -- The reach event status — a state machine.
    status text NOT NULL DEFAULT 'sent' CHECK (status IN (
        'sent',           -- message was sent to the channel
        'delivered',      -- confirmed delivery (e.g. push delivered, email accepted)
        'opened',         -- email opened / push seen
        'clicked',        -- link clicked
        'replied',        -- recipient replied (any disposition)
        'positive_reply', -- recipient replied positively
        'declined',       -- recipient explicitly declined
        'converted',      -- recipient became a fan
        'bounced',        -- delivery failed (hard bounce)
        'complained',     -- recipient marked as spam / complaint
        'ignored',        -- no response after observation window
        'failed'          -- internal error during send
    )),

    -- When the reach event was recorded and when its status was last updated.
    sent_at timestamptz NOT NULL DEFAULT now(),
    status_updated_at timestamptz NOT NULL DEFAULT now(),

    -- Fan conversion attribution: set when the recipient becomes a fan.
    converted_fan_id uuid REFERENCES fans(id) ON DELETE SET NULL,
    converted_at timestamptz,

    -- The episode this reach event belongs to (links to the episode model).
    episode_id text,

    -- Free-form metadata (content reference, subreddit name, etc.).
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,

    created_at timestamptz NOT NULL DEFAULT now()
);

-- Indexes for the brain's most common queries.
CREATE INDEX IF NOT EXISTS idx_reach_events_workspace_sent_at
    ON viryaos_reach_events (workspace_id, sent_at DESC);

CREATE INDEX IF NOT EXISTS idx_reach_events_workspace_channel_status
    ON viryaos_reach_events (workspace_id, channel, status);

CREATE INDEX IF NOT EXISTS idx_reach_events_workspace_template_status
    ON viryaos_reach_events (workspace_id, template_id, status);

CREATE INDEX IF NOT EXISTS idx_reach_events_action_id
    ON viryaos_reach_events (action_id)
    WHERE action_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_reach_events_converted_fan
    ON viryaos_reach_events (workspace_id, converted_fan_id)
    WHERE converted_fan_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_reach_events_recipient
    ON viryaos_reach_events (workspace_id, recipient_kind, recipient_id);

-- Ensure one reach event per (action_id, recipient_id, channel) — the same
-- action shouldn't create duplicate reach events for the same recipient on
-- the same channel.
CREATE UNIQUE INDEX IF NOT EXISTS uq_reach_events_action_recipient_channel
    ON viryaos_reach_events (action_id, recipient_id, channel)
    WHERE action_id IS NOT NULL;
