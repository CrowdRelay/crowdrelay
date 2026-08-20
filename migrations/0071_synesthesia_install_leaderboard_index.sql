-- Restore the leaderboard hot-path index to the current anonymous/install-based model.
--
-- Migration 0045 switched ranking to linked fans and replaced the install_hash
-- index with a fan_id index. The public leaderboard now ranks the best completed
-- published run per installation again, so keep the physical access path aligned
-- with the DISTINCT ON (install_hash) queries used by list/publish.

DROP INDEX IF EXISTS synesthesia_runs_leaderboard_fan_best_idx;

CREATE INDEX synesthesia_runs_leaderboard_best_idx
    ON synesthesia_runs (
        workspace_id,
        campaign_slug,
        install_hash,
        client_total_elapsed_ms,
        completed_at,
        id
    )
    WHERE completed_at IS NOT NULL
      AND client_total_elapsed_ms IS NOT NULL
      AND leaderboard_name IS NOT NULL;
