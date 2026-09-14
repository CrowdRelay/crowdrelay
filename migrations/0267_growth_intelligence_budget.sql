-- Raise the growth_intelligence action budget from 10/day to 30/day.
--
-- The context that loads the world model's raw material — scanner findings,
-- strategist reads, source signals — evaluated 32 decision candidates in a
-- single production day while its cap allowed 10 actions. The brain wanted
-- three times more intelligence than the policy permitted; the engine
-- idled on its actual mission while show_operations reminders consumed the
-- action share.
--
-- The update is pinned to the seeded default and skips rows an operator
-- disabled or a guardrail parked: their cap may be a choice, not a leftover.
-- The provisioning function is replaced so new tenants start at the
-- corrected budget.

UPDATE viryaos_autopilot_policies
SET max_actions_24h = 30
WHERE context = 'growth_intelligence'
  AND max_actions_24h = 10
  AND enabled
  AND (guarded_until IS NULL OR guarded_until <= now());

CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_autopilot_policies
        (workspace_id, context, max_actions_24h, enabled, autonomy_level)
    VALUES
        -- Drafting, discovery, measurement: unattended.
        (NEW.id, 'growth_intelligence', 30, true, 'bounded_auto'),
        (NEW.id, 'outreach_supply', 2, true, 'bounded_auto'),
        (NEW.id, 'content_supply', 30, true, 'bounded_auto'),
        (NEW.id, 'growth_metrics', 12, true, 'bounded_auto'),
        (NEW.id, 'growth_debt', 10, true, 'bounded_auto'),
        -- Spends money.
        (NEW.id, 'ticket_yield', 10, true, 'require_approval'),
        (NEW.id, 'merchandising', 20, true, 'require_approval'),
        (NEW.id, 'merch_pricing', 10, true, 'require_approval'),
        (NEW.id, 'merch_bundle', 5, true, 'require_approval'),
        (NEW.id, 'promotion_budget', 20, true, 'require_approval'),
        (NEW.id, 'funding', 10, true, 'require_approval'),
        -- Contacts a person.
        (NEW.id, 'fan_lifecycle', 100, true, 'require_approval'),
        (NEW.id, 'outreach', 20, true, 'require_approval'),
        (NEW.id, 'beacon', 12, true, 'require_approval'),
        -- Publishes or commits.
        (NEW.id, 'campaign_lifecycle', 20, true, 'require_approval'),
        (NEW.id, 'release', 30, true, 'require_approval'),
        (NEW.id, 'booking_opportunity', 10, true, 'require_approval'),
        (NEW.id, 'live_opportunity', 15, true, 'require_approval'),
        (NEW.id, 'show_operations', 50, true, 'require_approval'),
        (NEW.id, 'show_growth', 14, true, 'require_approval'),
        (NEW.id, 'experimentation', 10, true, 'require_approval'),
        (NEW.id, 'plays', 40, true, 'require_approval')
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;
