-- Release tier: the band's call about what kind of release this is
-- (CROWDRELAY_LEVERAGE_PLAN §4i-3).
--
--   single  — the full release vertical: every milestone, pre-save, push.
--   track   — announced, catalogued, one wave. The honest default.
--   filler  — posted into a quiet week; no vertical, no spend.
--
-- The tier is recorded so timing and outcome can be compared across releases
-- from the first one, and so the milestone evaluator knows a filler release
-- never owes the ladder.
ALTER TABLE viryaos_release_plans
    ADD COLUMN tier text NOT NULL DEFAULT 'track'
        CHECK (tier IN ('single','track','filler'));
