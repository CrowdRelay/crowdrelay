-- The public leaderboard dedupes best attempts per anonymous installation:
-- DISTINCT ON (install_hash) ... ORDER BY install_hash, elapsed, completed_at, id.
-- Migration 0045 replaced the install-scoped best index with a fan-scoped one
-- once runs became linkable to fans, but the unauthenticated list still groups
-- by install hash. Without this index every cache miss re-reads and re-sorts
-- all completed non-synthetic runs for the campaign on a public endpoint.

CREATE INDEX IF NOT EXISTS synesthesia_runs_leaderboard_install_best_idx
    ON synesthesia_runs (
        workspace_id,
        campaign_slug,
        install_hash,
        client_total_elapsed_ms,
        completed_at,
        id
    )
    WHERE NOT synthetic
      AND completed_at IS NOT NULL
      AND client_total_elapsed_ms IS NOT NULL
      AND leaderboard_name IS NOT NULL;
