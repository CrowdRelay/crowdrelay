-- Prestige the score cannot express on its own.
--
-- A Mystic or Pol'and'Rock slot is worth playing at break-even, and the score
-- before this migration has no way to say so: economics carries points and
-- prestige carries none. This column is that value, confirmed by an operator.
--
-- Defaults to zero (Standard tier). A name match against a landmark promoter
-- or festival list is a suggestion an operator confirms, never an automatic
-- grant — "Festival" in a title means nothing on its own — so this column is
-- written only by an explicit operator action, never by discovery.

ALTER TABLE viryaos_team_opportunities
    ADD COLUMN strategic_value_basis_points integer NOT NULL DEFAULT 0
        CHECK (strategic_value_basis_points BETWEEN 0 AND 10000);
