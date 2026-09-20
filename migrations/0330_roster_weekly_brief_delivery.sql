-- Roster weekly brief delivery: the measured read becomes an issued
-- artifact, the way the daily briefing did.
--
-- `roster_weekly_brief::act_briefs` already measures one organisation's
-- acts — pending decisions, slipped asks, posture, North Star delta — but
-- nothing ever delivered it: the admin endpoint answered only when an
-- operator remembered to ask. This migration creates the artifact the
-- sweep issues once per organisation per week: one brief per
-- organisation per issuer-local Monday, composed from the same measured
-- read the endpoint serves, delivered to owner/admin members of the
-- member workspaces through the team-handoff rail (assignment + email).
--
-- `local_date` mirrors `viryaos_daily_briefings`: the civil Monday the
-- brief speaks for, in the issuing workspace's tenant zone. ISO weeks
-- make the label identical for every member workspace — two workspaces
-- in different zones cannot disagree about which week a Monday belongs
-- to — so (organization_id, local_date) is a safe dedupe across the
-- per-workspace workers that race to issue it.
--
-- `source_kind 'roster_weekly_brief'` widens the assignment vocabulary
-- the same way 'daily_briefing' did; `context 'roster'` widens the
-- autopilot vocabulary on all three tables that carry it, because the
-- queued team-email decision/action rows need a context that names the
-- domain.

CREATE TABLE viryaos_roster_briefs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id uuid NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    -- The issuer-local Monday the brief speaks for — the week key, not a
    -- per-day key: one row per organisation per ISO week.
    local_date date NOT NULL,
    title text NOT NULL CHECK (btrim(title) <> '' AND char_length(title) <= 200),
    body text NOT NULL CHECK (btrim(body) <> '' AND char_length(body) <= 4000),
    -- Counts behind the body — {"acts":5,"acts_with_pending":2,...} — so an
    -- operator can ask "did the brief have anything in it" without parsing
    -- prose. Same contract as viryaos_daily_briefings.sections.
    sections jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(sections) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, local_date)
);

COMMENT ON TABLE viryaos_roster_briefs IS
    'One weekly roster brief per organisation per issuer-local Monday, '
    'delivered to owner/admin members of member workspaces. The artifact; '
    'delivery rides viryaos_team_assignments like the daily briefing.';

ALTER TABLE viryaos_team_assignments
    DROP CONSTRAINT viryaos_team_assignments_source_kind_check;
ALTER TABLE viryaos_team_assignments
    ADD CONSTRAINT viryaos_team_assignments_source_kind_check
    CHECK (source_kind IN (
        'autopilot_action','show_task','opportunity','beacon','capture_plan',
        'daily_briefing','release_making_of','roster_weekly_brief'
    ));

-- The same per-member dedupe `one_briefing_per_member` gives the daily
-- briefing: one assignment per member per issued brief, so a retried
-- insert can never hand a manager the same week's page twice.
CREATE UNIQUE INDEX viryaos_team_assignments_one_roster_brief_per_member
    ON viryaos_team_assignments (workspace_id, source_id, assignee_member_id)
    WHERE source_kind = 'roster_weekly_brief';

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
        'growth_intelligence','content_strategy','representation',
        'booking_agent','roster'
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
        'growth_intelligence','content_strategy','representation',
        'booking_agent','roster'
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
        'growth_intelligence','content_strategy','representation',
        'booking_agent','roster'
    ));

-- The context contract is three-sided: CHECKs, the Rust enum, and this
-- trigger's per-workspace provisioning list. A workspace created tomorrow
-- must get a 'roster' policy row the same way it gets the other 25, and
-- today's workspaces need the backfill the trigger cannot reach. The row
-- sits with the person-contacting family: the brief writes to a manager's
-- inbox, so an inherited require_approval posture is the honest default.
INSERT INTO viryaos_autopilot_policies
    (workspace_id, context, max_actions_24h, enabled, autonomy_level,
     minimum_confidence_basis_points)
SELECT id, 'roster', 10, true, 'require_approval', 8000
FROM workspaces
ON CONFLICT (workspace_id, context) DO NOTHING;

CREATE OR REPLACE FUNCTION viryaos_provision_autopilot_policies()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO viryaos_autopilot_policies
        (workspace_id, context, max_actions_24h, enabled, autonomy_level,
         minimum_confidence_basis_points)
    VALUES
        -- Drafting, discovery, measurement: unattended.
        (NEW.id, 'growth_intelligence', 30, true, 'bounded_auto', 8000),
        (NEW.id, 'outreach_supply', 2, true, 'bounded_auto', 8000),
        (NEW.id, 'content_supply', 30, true, 'bounded_auto', 8000),
        (NEW.id, 'growth_metrics', 12, true, 'bounded_auto', 8000),
        (NEW.id, 'growth_debt', 10, true, 'bounded_auto', 8000),
        -- Spends money.
        (NEW.id, 'ticket_yield', 10, true, 'require_approval', 8000),
        (NEW.id, 'merchandising', 20, true, 'require_approval', 8000),
        (NEW.id, 'merch_pricing', 10, true, 'require_approval', 8000),
        (NEW.id, 'merch_bundle', 5, true, 'require_approval', 8000),
        (NEW.id, 'promotion_budget', 20, true, 'require_approval', 8000),
        (NEW.id, 'funding', 10, true, 'require_approval', 8000),
        -- Contacts a person.
        (NEW.id, 'fan_lifecycle', 100, true, 'require_approval', 8000),
        (NEW.id, 'outreach', 20, true, 'require_approval', 8000),
        (NEW.id, 'beacon', 12, true, 'require_approval', 8000),
        -- The organisation's own rail: the weekly brief writes to a
        -- manager's inbox, so it sits with the person-contacting family
        -- rather than the unattended measurement rows.
        (NEW.id, 'roster', 10, true, 'require_approval', 8000),
        -- The band approaching an agent or a label — brokered, hidden
        -- address, scarce by the month.
        (NEW.id, 'representation', 4, true, 'require_approval', 5000),
        -- The band approaching a booking agent for representation —
        -- scarcer still: one letter per agent a season, and the evidence
        -- gate binds harder than this cap ever will.
        (NEW.id, 'booking_agent', 2, true, 'require_approval', 5000),
        -- Publishes or commits.
        (NEW.id, 'campaign_lifecycle', 20, true, 'require_approval', 8000),
        (NEW.id, 'release', 30, true, 'require_approval', 8000),
        (NEW.id, 'booking_opportunity', 10, true, 'require_approval', 8000),
        (NEW.id, 'live_opportunity', 15, true, 'require_approval', 8000),
        (NEW.id, 'show_operations', 50, true, 'require_approval', 8000),
        (NEW.id, 'show_growth', 14, true, 'require_approval', 8000),
        (NEW.id, 'experimentation', 10, true, 'require_approval', 8000),
        (NEW.id, 'plays', 40, true, 'require_approval', 8000),
        -- The band decides what it makes. Creative work stays human in
        -- every posture: the engine proposes, the band commits. The 5000
        -- floor matches the evaluator's evidence-derived confidence — a
        -- bare raise scores 6000 — where the inherited 8000 default would
        -- deny most of what the engine ranked.
        (NEW.id, 'content_strategy', 6, true, 'require_approval', 5000)
    ON CONFLICT (workspace_id, context) DO NOTHING;
    RETURN NEW;
END;
$$;
