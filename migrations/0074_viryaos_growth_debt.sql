-- Growth debt: the Autopilot context for work the business already committed
-- to and then left undone.
--
-- Deliberately schema-light. Every input this context reasons about is a
-- first-party row that already exists — booking and outreach targets and their
-- interaction logs, Beacons, show-growth surfaces, release plans and their
-- milestones — so there is nothing here to store. Adding a `viryaos_growth_debt`
-- table would create a second, immediately stale copy of facts the owning
-- tables already hold; the debt is derived at evaluation time instead, exactly
-- like a `growth_metrics` trend.
--
-- Why a separate context rather than predicates on `outreach`, `release` and
-- `show_growth`, which is what the plan's default said to prefer: those
-- contexts execute contractual, outward-facing work (a booking email, a press
-- pitch, a release announcement) and are quota'd and authority-gated for that.
-- Raising debt is an observation about our own records and is safe to run at a
-- far wider scope. Folding it in would either widen those contexts' authority
-- to cover a cheap observation, or throttle the observation behind a quota
-- sized for paid outreach. It also needs its own 24h budget: how often an
-- operator wants to be told about neglect is not the same number as how many
-- emails may go out. The action it emits is `growth.debt.raise` for every kind,
-- so the ranked queue in Phase 4 sees one comparable stream.
--
-- `subject_kind` on the decision and action tables is free-form bounded text
-- (migration 0033), so the new booking_target / outreach_target subjects need
-- no constraint change here.

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt'
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
        'growth_metrics','growth_debt'
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
        'growth_metrics','growth_debt'
    ));

-- Provisioned disabled and at 'observe' like every other context. The quota is
-- 10/day: debt is slow-moving and each subject is silenced for two weeks after
-- it is raised, so a higher ceiling would only buy noise.
INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT id, 'growth_debt', 10 FROM workspaces
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
        (NEW.id, 'growth_debt', 10)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;
