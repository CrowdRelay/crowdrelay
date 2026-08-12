-- Synesthesia public scores are ranked per linked fan, not per installation.
--
-- Publication remains opt-in and `leaderboard_name` stores only a server-made
-- masked e-mail alias (for example `woj••••`). The full e-mail remains
-- private in the existing fan identity tables and is never returned publicly.
-- Ranking per fan prevents one person using multiple devices from occupying
-- multiple places and aligns the hot DISTINCT ON path with its index.

DROP INDEX IF EXISTS synesthesia_runs_leaderboard_best_idx;

CREATE INDEX synesthesia_runs_leaderboard_fan_best_idx
    ON synesthesia_runs (
        workspace_id,
        campaign_slug,
        fan_id,
        client_total_elapsed_ms,
        completed_at,
        id
    )
    WHERE fan_id IS NOT NULL
      AND completed_at IS NOT NULL
      AND client_total_elapsed_ms IS NOT NULL
      AND leaderboard_name IS NOT NULL;
