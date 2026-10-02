-- FAN SCOUT proactive person discovery outcome.
--
-- Core lands the reader before the agents service starts writing this kind.
-- It is internal research only: no action/authority is created. The worker
-- resolves every selected candidate_ref against deterministic task metadata
-- before the canonical fan_prospects spine is touched.

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
        'contact_research',
        'fan_prospects'
    ));
