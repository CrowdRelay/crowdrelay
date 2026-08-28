-- Growth Intelligence context: the deterministic brain.
--
-- The brain is deterministic Rust machinery (the autopilot evaluator). It
-- decides what intelligence to gather, when, and what to do with it. LLMs
-- are workers/tools that gather intelligence and draft content. The brain
-- dispatches workers via `RequestAgentRun` actions, which create rows in
-- `agent_service_tasks` (owned by the TS agent service).
--
-- This migration only adds the `growth_intelligence` context to the autopilot
-- policy/decision/action CHECK constraints and provisions existing workspaces.
-- The `agent_service_tasks` table already exists (created by the TS agent
-- service migrations), and the `RequestAgentRun` action kind is a free-form
-- string (no CHECK constraint on `action_kind`).

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt','outreach_supply','plays',
        'growth_intelligence'
    ));

ALTER TABLE viryaos_autopilot_decisions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_decisions_context_check;
ALTER TABLE viryaos_autopilot_decisions
    ADD CONSTRAINT viryaos_autopilot_decisions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt','outreach_supply','plays',
        'growth_intelligence'
    ));

ALTER TABLE viryaos_autopilot_actions
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_actions_context_check;
ALTER TABLE viryaos_autopilot_actions
    ADD CONSTRAINT viryaos_autopilot_actions_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt','outreach_supply','plays',
        'growth_intelligence'
    ));

-- Provisioned disabled and at 'observe' like every other context: the brain
-- must be watched before it is allowed to dispatch workers. The quota is
-- 10/day: the brain dispatches on deterministic cooldowns (2-7 days per
-- template), so a higher ceiling would only buy noise.
INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT id, 'growth_intelligence', 10 FROM workspaces
ON CONFLICT (workspace_id, context) DO NOTHING;

CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
    VALUES
        (NEW.id, 'ticket_yield', 10),
        (NEW.id, 'fan_lifecycle', 100),
        (NEW.id, 'campaign_lifecycle', 20),
        (NEW.id, 'merchandising', 20),
        (NEW.id, 'merch_pricing', 10),
        (NEW.id, 'merch_bundle', 5),
        (NEW.id, 'booking_opportunity', 10),
        (NEW.id, 'outreach', 20),
        (NEW.id, 'content_supply', 30),
        (NEW.id, 'promotion_budget', 20),
        (NEW.id, 'experimentation', 10),
        (NEW.id, 'show_operations', 50),
        (NEW.id, 'release', 30),
        (NEW.id, 'live_opportunity', 15),
        (NEW.id, 'funding', 10),
        (NEW.id, 'beacon', 12),
        (NEW.id, 'show_growth', 14),
        (NEW.id, 'growth_metrics', 12),
        (NEW.id, 'growth_debt', 10),
        (NEW.id, 'outreach_supply', 2),
        (NEW.id, 'plays', 40),
        (NEW.id, 'growth_intelligence', 10)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;
