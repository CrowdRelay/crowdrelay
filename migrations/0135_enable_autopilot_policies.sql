-- Enable all autopilot policies for the Virya workspace and set growth-relevant
-- contexts to bounded_auto so the brain can dispatch LLM workers without
-- manual approval. Other contexts stay at their current autonomy level
-- (require_approval or observe) — they can draft but not auto-execute.
--
-- This unblocks the growth loop: the brain runs every 5 minutes but was
-- filtering out all disabled policies, producing zero decisions. With
-- policies enabled, the brain will dispatch reddit-scanner, community-engager,
-- press-pitch, social-post, etc. on their configured cooldowns.

-- Virya workspace ID.
DO $$
BEGIN
    UPDATE viryaos_autopilot_policies
    SET enabled = true, updated_at = now()
    WHERE workspace_id = '6c69282c-0d60-4f18-8379-60ede34362c6';

    -- Growth-relevant contexts: the brain auto-dispatches LLM workers.
    -- LLM workers only draft content — posting is still manual or bounded
    -- by the community executor's safety rails.
    UPDATE viryaos_autopilot_policies
    SET autonomy_level = 'bounded_auto', updated_at = now()
    WHERE workspace_id = '6c69282c-0d60-4f18-8379-60ede34362c6'
      AND context IN ('growth_intelligence', 'outreach_supply', 'promotion_budget');
END $$;
