-- The research agent's outcome kind.
--
-- The `contact-researcher` template emits kind = 'contact_research'. Without the
-- value in this CHECK every research outcome INSERT fails, and because the
-- agents service writes outcomes in the same transaction that completes the
-- task, the whole run dies with it: result lost, premium tokens spent, nothing
-- on file. Same shape as 0394 for the event scout; adding a value to a CHECK
-- only widens what an insert may carry, so no existing row can violate it.

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
        'generic_insight',
        'opportunity_findings',
        'strategy_proposals',
        'beacon_candidates',
        'contact_research'
    ));
