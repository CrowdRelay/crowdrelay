-- Add 'signal_push' to the agent_outcomes.kind CHECK constraint.
--
-- Migration 0125 created the table with 7 kinds. The SignalPush outcome kind
-- was added to the Rust/TS schemas but the DB constraint was never updated,
-- so the agents service gets a CHECK violation when writing a signal_push
-- outcome. This fixes that gap.

ALTER TABLE agent_outcomes
    DROP CONSTRAINT IF EXISTS agent_outcomes_kind_check,
    ADD CONSTRAINT agent_outcomes_kind_check CHECK (kind IN (
        'press_pitch',
        'social_post',
        'signal_push',
        'audience_segments',
        'outreach_targets',
        'campaign_insight',
        'release_plan_note',
        'generic_insight'
    ));
