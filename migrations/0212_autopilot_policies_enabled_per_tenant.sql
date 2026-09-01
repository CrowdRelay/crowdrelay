-- Give every tenant a working brain, not just the one whose UUID was typed in.
--
-- `viryaos_autopilot_policies` defaults to `enabled = false` and
-- `autonomy_level = 'observe'`, and the provisioning trigger seeds rows without
-- touching either. The only thing that ever enabled anything was migration
-- 0135, which hardcoded a single workspace id:
--
--     WHERE workspace_id = '6c69282c-0d60-4f18-8379-60ede34362c6';
--
-- So a tenant onboarded through the wizard got a brain that loaded a world
-- model, chose a strategy, and then dropped every action on the floor — the
-- quota check reads `WHERE ... AND enabled` and returns Conflict when no
-- enabled row matches. The autopilot appeared to run and produced nothing, with
-- no error an operator would ever see.
--
-- Autonomy is assigned by consequence, not convenience:
--
--   bounded_auto — drafting, discovery and measurement. An LLM worker writes
--       a draft or a scanner records a finding. Nothing is sent, published,
--       priced or charged. Posting stays behind the community executor's own
--       rails regardless of this setting.
--
--   require_approval — everything that spends money, contacts a person, or
--       publishes. The brain prepares the action and an operator decides. This
--       is the attention inbox doing its job, not the brain going quiet.
--
-- Nothing is set to `observe`: an enabled policy at observe measures what would
-- have happened and emits nothing, which is indistinguishable from the broken
-- state this migration fixes. A tenant who wants silence can disable a context
-- explicitly, and that choice is preserved below.

-- ---------------------------------------------------------------------------
-- 1. Backfill every existing workspace.
-- ---------------------------------------------------------------------------

-- `version` increments on every operator edit, so `version = 1` is a row nobody
-- has touched. A context an operator deliberately disabled or guarded keeps
-- that decision: this backfill only moves rows still sitting at the seeded
-- default, and never one under an active guardrail.
UPDATE viryaos_autopilot_policies
SET enabled = true, version = version + 1, updated_at = now()
WHERE NOT enabled
  AND version = 1
  AND (guarded_until IS NULL OR guarded_until <= now());

-- Drafting, discovery and measurement run unattended.
UPDATE viryaos_autopilot_policies
SET autonomy_level = 'bounded_auto', version = version + 1, updated_at = now()
WHERE autonomy_level = 'observe'
  AND version <= 2
  AND (guarded_until IS NULL OR guarded_until <= now())
  AND context IN (
      'growth_intelligence',
      'outreach_supply',
      'content_supply',
      'growth_metrics',
      'growth_debt'
  );

-- Everything with a consequence proposes and waits.
UPDATE viryaos_autopilot_policies
SET autonomy_level = 'require_approval', version = version + 1, updated_at = now()
WHERE autonomy_level = 'observe'
  AND version <= 2
  AND (guarded_until IS NULL OR guarded_until <= now());

-- ---------------------------------------------------------------------------
-- 2. Give new workspaces the same shape at provisioning time.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_autopilot_policies
        (workspace_id, context, max_actions_24h, enabled, autonomy_level)
    VALUES
        -- Drafting, discovery, measurement: unattended.
        (NEW.id, 'growth_intelligence', 10, true, 'bounded_auto'),
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
