-- Content Strategy context: the suggestion engine's route to the operator.
--
-- 3.5b.5 ranks the catalogue into the two or three beats worth making and
-- persists them as `raised` rows in viryaos_content_suggestions. This
-- context surfaces each raised suggestion as a `content.suggestion.raise`
-- action in the same approval queue every other finding uses — same
-- evidence panel, same floor, same cap. Approving the action marks the
-- suggestion `approved`: the band committed to the beat. Cancelling it
-- resolves the suggestion `declined` with its outcome row, because "not
-- for us" is a first-class taste signal, not a dismissal.
--
-- Provisioned enabled at require_approval: a suggestion is creative work
-- only the band can judge, so every posture keeps a human on it. The cap
-- is 6/day against an open queue of at most three suggestions — headroom
-- for a churned queue, never a feed.
--
-- The confidence floor is set explicitly at 5000 rather than inheriting the
-- table default 8000: the evaluator's confidence is evidence-derived — a
-- bare raise scores 6000 and corroboration lifts it from there — so the
-- default floor would deny most of what the engine ranked, writing
-- invisible `deny` decisions while the suggestion sat open forever. The
-- ask-gate is the EFE score; the floor only refuses a row that somehow
-- arrives carrying no evidence at all.

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
        'growth_intelligence','content_strategy'
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
        'growth_intelligence','content_strategy'
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
        'growth_intelligence','content_strategy'
    ));

INSERT INTO viryaos_autopilot_policies
    (workspace_id, context, max_actions_24h, enabled, autonomy_level,
     minimum_confidence_basis_points)
SELECT id, 'content_strategy', 6, true, 'require_approval', 5000
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
