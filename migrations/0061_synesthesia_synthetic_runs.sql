-- Production-safe synthetic Synesthesia E2E runs.
-- Synthetic journeys exercise the real public run lifecycle but are excluded
-- from audience/leaderboard/reward business signals and can never be linked to
-- a fan. This keeps production black-box validation from polluting metrics.

ALTER TABLE synesthesia_runs
    ADD COLUMN synthetic boolean NOT NULL DEFAULT false;

CREATE INDEX synesthesia_runs_synthetic_cleanup_idx
    ON synesthesia_runs (updated_at, id)
    WHERE synthetic;
