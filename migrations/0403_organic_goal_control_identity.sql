-- The monthly organic scoreboard now participates in decision provenance.
-- Give each month a stable identity rather than minting a different UUID on
-- every read. Existing 0402 rows receive one identity exactly once.
ALTER TABLE organic_fan_goals
    ADD COLUMN id uuid NOT NULL DEFAULT gen_random_uuid();

CREATE UNIQUE INDEX organic_fan_goals_id_key
    ON organic_fan_goals (id);
