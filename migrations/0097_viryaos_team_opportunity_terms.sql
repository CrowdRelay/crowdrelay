-- Talking terms, once a promoter has actually named a fee.
--
-- Phase 7 works out what a show costs and Phase 8a and 8b decide whether it is
-- worth pursuing. This is the part after that, and it has been the slowest
-- manual work in the whole booking loop: reading an offer, working out what it
-- leaves after the drive and the rooms, and deciding what to ask for instead.
--
-- The table is the negotiation, not the opportunity. One live negotiation per
-- opportunity, because two rows disagreeing about what has been offered is a
-- band making two different promises to the same promoter.
--
-- The ladder is frozen when the negotiation opens. A ladder that moved under a
-- running conversation would make the counter sent last week unexplainable
-- from the row today. The acceptance rule still re-reads the *current* costed
-- margin, so a ladder frozen against numbers that later turn out to be wrong
-- cannot talk the agent into a show that no longer clears.
--
-- What the agent may do here is bounded by the class ceiling and not by this
-- schema: at the current posture a counter and an acceptance are both
-- `third_party`, so both are drafted and parked for a human. Declining and
-- expiring are settlements rather than outward moves — the agent records that
-- it will not take these terms, and telling the promoter stays a human act.

CREATE TABLE viryaos_team_opportunity_terms (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    opportunity_id uuid NOT NULL,
    state text NOT NULL DEFAULT 'proposed' CHECK (state IN (
        'proposed', 'countered', 'accepted', 'declined', 'expired'
    )),
    currency char(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    -- What the promoter has on the table right now.
    offered_fee_minor bigint NOT NULL CHECK (offered_fee_minor >= 0),
    -- The three numbers the conversation is conducted against, frozen at open.
    walk_away_minor bigint NOT NULL CHECK (walk_away_minor >= 0),
    target_minor bigint NOT NULL CHECK (target_minor >= walk_away_minor),
    opening_ask_minor bigint NOT NULL CHECK (opening_ask_minor >= target_minor),
    -- The agent's last ask, and how many it has made. Bounded here as well as
    -- in the domain: a row claiming nine rounds is a row no code path wrote.
    countered_fee_minor bigint CHECK (countered_fee_minor IS NULL OR countered_fee_minor >= 0),
    counter_rounds integer NOT NULL DEFAULT 0 CHECK (counter_rounds BETWEEN 0 AND 8),
    -- When the promoter's side of this goes cold. Terms agreed after they
    -- stopped waiting are not terms.
    responds_by timestamptz NOT NULL,
    settled_at timestamptz,
    -- Present only on a negotiation that ended without an acceptance. This is
    -- what makes a refusal auditable: `requires_contract` on a well-paid offer
    -- says the agent refused something it was never allowed to take, which is
    -- a different fact from the money being wrong.
    settled_reason text CHECK (settled_reason IS NULL OR settled_reason IN (
        'below_floor', 'requires_contract', 'exclusive', 'date_not_free',
        'past_annual_stretch', 'stretch_score_too_low', 'cost_insufficient',
        'promoter_withdrew', 'window_closed'
    )),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_team_opportunity_terms_opportunity_fk
        FOREIGN KEY (workspace_id, opportunity_id)
        REFERENCES viryaos_team_opportunities (workspace_id, id)
        ON DELETE CASCADE,
    UNIQUE (workspace_id, id),
    -- One negotiation per opportunity, for ever. Reopening is an operator
    -- deliberately recording a new offer, never a state machine deciding on its
    -- own that a settled conversation is live again.
    UNIQUE (workspace_id, opportunity_id),
    CHECK ((settled_at IS NOT NULL) = (state IN ('accepted', 'declined', 'expired'))),
    -- A reason on an unsettled negotiation describes something that has not
    -- happened, and an accepted one needs no reason.
    CHECK (settled_reason IS NULL OR settled_at IS NOT NULL),
    CHECK (state <> 'accepted' OR settled_reason IS NULL),
    -- Countered means an ask is outstanding, so there has to be one.
    CHECK (state <> 'countered' OR countered_fee_minor IS NOT NULL)
);

CREATE TRIGGER viryaos_team_opportunity_terms_set_updated_at
BEFORE UPDATE ON viryaos_team_opportunity_terms
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

-- The cycle asks "which negotiations are still live" every few minutes.
-- Partial, because a settled one is never looked at again.
CREATE INDEX viryaos_team_opportunity_terms_live_idx
    ON viryaos_team_opportunity_terms (workspace_id, responds_by)
    WHERE settled_at IS NULL;
