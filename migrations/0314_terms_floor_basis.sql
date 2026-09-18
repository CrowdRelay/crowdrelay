-- The floor's citation (§4h-9, Sprint 4.9): which input produced the frozen
-- walk-away, and the two external numbers that stood behind it.
--
-- `floor_basis` names the bound input — the costed trip, the market evidence
-- pooled across every room the counterparty works, or the counterparty's own
-- last accepted fee — so the frozen ladder cites its source rather than only
-- its number, and the drafted counter can say which. `prior_fee_minor` and
-- `market_floor_minor` freeze those inputs on the row alongside it: the
-- terms_countered / terms_accepted payload carries them verbatim, which a
-- recompute at execution could never do honestly — evidence moves under a
-- running negotiation, and a citation that moved with it would not be the
-- number the ladder was argued from.
--
-- Existing rows keep 'cost': every ladder written before this column was the
-- bare cost floor — no other input existed to bind. NULL minors are the
-- honest absence: no prior precedent, no cleared venue band.
ALTER TABLE viryaos_team_opportunity_terms
    ADD COLUMN floor_basis text NOT NULL DEFAULT 'cost'
        CHECK (floor_basis IN ('cost','market','counterparty_history')),
    ADD COLUMN prior_fee_minor bigint
        CHECK (prior_fee_minor IS NULL OR prior_fee_minor >= 0),
    ADD COLUMN market_floor_minor bigint
        CHECK (market_floor_minor IS NULL OR market_floor_minor >= 0);
