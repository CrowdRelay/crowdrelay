-- The decision that keeps the pitcher from looping over an empty table.
--
-- Phase 9 built candidate ingestion, screening, dedupe, refusal and promotion,
-- and every one of those parts works. None of them ever runs, because nothing
-- decides to go looking: `POST /v1/admin/autopilot/outreach/candidates` is
-- inbound only, so something outside the agent has to want supply before the
-- agent can have any. In production that left `viryaos_outreach_targets` at
-- zero rows while `outreach.send` sat advertised and idle — a live execution
-- path starving next to a working screening pipeline with nothing in it.
--
-- `outreach_supply` closes that. It is the context that notices the pitcher is
-- short and asks an adapter to sweep published sources, and it is deliberately
-- the smallest possible thing that turns zero targets from a stable state into
-- a problem the agent can see.
--
-- Three properties are worth stating because they are what make it safe to run
-- unattended:
--
-- 1. The action reads public data, contacts nobody and buys nothing, so it is
--    `first_party_reversible` and needs no new autonomy. Every judgement about
--    what may be contacted stays in screening, where it already is.
-- 2. It holds when candidates are piling up unconfirmed. Fetching more supply
--    while a human queue is full would hide an operator bottleneck behind
--    apparent activity, which is the failure mode that makes an autonomous
--    system feel busy and change nothing.
-- 3. It stops after repeated barren sweeps. Widening a dry source is an
--    operator decision; asking a third time is the autonomous equivalent of
--    refreshing an empty inbox.
--
-- Provisioned disabled and at 'observe', like every context before it. Turning
-- it on is a deliberate operator act, not a side effect of deploying.

ALTER TABLE viryaos_autopilot_policies
    DROP CONSTRAINT IF EXISTS viryaos_autopilot_policies_context_check;
ALTER TABLE viryaos_autopilot_policies
    ADD CONSTRAINT viryaos_autopilot_policies_context_check CHECK (context IN (
        'ticket_yield','fan_lifecycle','campaign_lifecycle',
        'merchandising','merch_pricing','merch_bundle',
        'booking_opportunity','outreach','content_supply',
        'promotion_budget','experimentation','show_operations',
        'release','live_opportunity','funding','beacon','show_growth',
        'growth_metrics','growth_debt','outreach_supply'
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
        'growth_metrics','growth_debt','outreach_supply'
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
        'growth_metrics','growth_debt','outreach_supply'
    ));

-- One sweep a day at most, so the quota is 2: enough to allow a retry after a
-- failed emission, far too few to turn a cooldown bug into a crawl.
INSERT INTO viryaos_autopilot_policies (workspace_id, context, max_actions_24h)
SELECT workspace.id, 'outreach_supply', 2
FROM workspaces workspace
ON CONFLICT (workspace_id, context) DO NOTHING;

-- Finding the most recent sweep, and the window each sweep owns, is the only
-- query this context runs; without the index it is a scan of every action the
-- agent has ever taken.
CREATE INDEX IF NOT EXISTS viryaos_autopilot_actions_discovery_sweep_idx
    ON viryaos_autopilot_actions (workspace_id, created_at DESC)
    WHERE action_kind = 'outreach.discovery.request';

-- The supply read counts pitchable targets on every cycle. Partial, because
-- do-not-contact and inactive rows are exactly the ones it must never count.
CREATE INDEX IF NOT EXISTS viryaos_outreach_targets_pitchable_idx
    ON viryaos_outreach_targets (workspace_id)
    WHERE active AND accepts_outreach AND NOT do_not_contact;
