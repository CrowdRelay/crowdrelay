-- Representation contacts and the consent they carry (§4h-12, steps c-e).
--
-- A booking agent or a label is a relationship target like press or radio,
-- so they live in `viryaos_outreach_targets` — the dispatch machinery
-- (consent flags, contact windows, interactions, reach ledger) is already
-- there. What differs is consent: a playlist's published submissions route
-- is an open invitation to pitch, while a researched agent's address is
-- not. Two rules follow.
--
-- `accepts_outreach` on an agent/label row must carry a basis — the stated
-- reason this contact accepts approaches from this band ("we met at the
-- showcase", "their site says send demos"). The flag is the contract; the
-- basis is what makes the flag mean something instead of being a box a
-- tired operator ticks. Other kinds keep the existing posture: a verified
-- published contact route is what `verified` already asserts.
--
-- `agent_outreach_targets.accepts_outreach` defaulted false and was never
-- written or read — the press-recipient path pitched `promoted` *and*
-- `proposed` rows without checking it or `do_not_contact`. The fix makes
-- the flag bind: promotion is the moment an operator verified a published
-- pitch route, which for press-side kinds is exactly "this contact accepts
-- outreach". Rows already promoted keep working; rows merely proposed do
-- not get mailed until an operator promotes them.

ALTER TABLE viryaos_outreach_targets
    DROP CONSTRAINT IF EXISTS viryaos_outreach_targets_target_kind_check;
ALTER TABLE viryaos_outreach_targets
    ADD CONSTRAINT viryaos_outreach_targets_target_kind_check CHECK (target_kind IN (
        'playlist','radio','press','creator','support_slot','endorsement','media_patronage',
        'agent','label'
    ));

ALTER TABLE viryaos_outreach_targets
    ADD COLUMN IF NOT EXISTS accepts_outreach_basis text
    CHECK (accepts_outreach_basis IS NULL
        OR (btrim(accepts_outreach_basis) <> '' AND char_length(accepts_outreach_basis) <= 240));

-- An agent or a label can only be marked as accepting approaches when the
-- basis for that belief is written down. The CHECK is the backstop; the
-- honest place is the endpoint that refuses to set the flag without one.
ALTER TABLE viryaos_outreach_targets
    ADD CONSTRAINT viryaos_outreach_targets_acceptance_check
    CHECK (
        target_kind NOT IN ('agent','label')
        OR NOT accepts_outreach
        OR accepts_outreach_basis IS NOT NULL
    );

-- Promoted contacts were operator-verified as real published routes; for
-- the personal-contact kinds a published route is the consent. Proposed
-- rows are research guesses and stay consented-out.
UPDATE agent_outreach_targets
SET accepts_outreach = true
WHERE status = 'promoted'
  AND contact_email IS NOT NULL
  AND target_kind IN ('press','radio','playlist','media_patronage','endorsement','creator');

-- A band's own mail and sheets hold the agents and labels it already
-- dealt with — first-party contacts that route into the representation
-- list, where the opt-in still has to be stated before an approach sends.
ALTER TABLE viryaos_drive_contacts
    DROP CONSTRAINT IF EXISTS viryaos_drive_contacts_suggested_kind_check;
ALTER TABLE viryaos_drive_contacts
    ADD CONSTRAINT viryaos_drive_contacts_suggested_kind_check
    CHECK (suggested_kind IS NULL OR suggested_kind IN (
        'fan', 'press', 'radio', 'playlist', 'media_patronage', 'endorsement',
        'creator', 'promoter', 'venue', 'festival', 'agent', 'label'
    ));

-- The approach is its own interaction phase: it is not an 'initial' pitch
-- of a release and not a 'followup', and the monthly allowance counts
-- exactly these rows. A distinct phase keeps the count honest rather than
-- pattern-matching source keys.
ALTER TABLE viryaos_outreach_interactions
    DROP CONSTRAINT IF EXISTS viryaos_outreach_interactions_phase_check;
ALTER TABLE viryaos_outreach_interactions
    ADD CONSTRAINT viryaos_outreach_interactions_phase_check
    CHECK (phase IN ('initial', 'followup', 'reply', 'approach'));

-- `representation` is the autopilot context for band-initiated approaches
-- to agents and labels. Same widening on all three tables — a decision and
-- its action carry the same context, and the policy row for it is seeded
-- below at require_approval: a real-world introduction always passes a
-- person before it leaves.
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
        'growth_intelligence','content_strategy','representation'
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
        'growth_intelligence','content_strategy','representation'
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
        'growth_intelligence','content_strategy','representation'
    ));

INSERT INTO viryaos_autopilot_policies
    (workspace_id, context, max_actions_24h, enabled, autonomy_level,
     minimum_confidence_basis_points)
SELECT id, 'representation', 4, true, 'require_approval', 5000
FROM workspaces
ON CONFLICT (workspace_id, context) DO NOTHING;

-- New workspaces get the same posture: an approach is a real-world
-- introduction the platform brokers, so it always passes a person first.
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
        -- The band approaching an agent or a label — brokered, hidden
        -- address, scarce by the month.
        (NEW.id, 'representation', 4, true, 'require_approval', 5000),
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
