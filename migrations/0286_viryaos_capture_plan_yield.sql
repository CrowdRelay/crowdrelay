-- Capture-plan yield: what a production day actually produced.
--
-- The settle pass already counts the sources a day harvested to reach
-- its verdict — then threw the number away. Persisting it makes the
-- leverage visible: the briefing can say "the shoot produced 4 of the
-- 5 planned pieces" instead of only "the plan is done". NULL means the
-- pass has not measured the plan yet — a missing number is never a
-- guessed zero.

ALTER TABLE viryaos_capture_plans
    ADD COLUMN sources_landed INTEGER
    CHECK (sources_landed IS NULL OR sources_landed >= 0);
