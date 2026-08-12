-- Synesthesia replay attempts + public opt-in leaderboard.
--
-- Each local journey gets its own attempt_id, so resetting/replaying the album no
-- longer reuses a previously completed run. Leaderboard publication is explicit:
-- a completed run token may publish one bounded pseudonym for the installation.
-- Public reads expose only pseudonym, rank and elapsed time; install/run hashes,
-- fan identity and reward e-mail never leave the API.

ALTER TABLE synesthesia_runs
    ADD COLUMN attempt_id text NOT NULL DEFAULT 'legacy'
        CHECK (
            char_length(attempt_id) BETWEEN 1 AND 64
            AND attempt_id ~ '^[a-zA-Z0-9_-]+$'
        ),
    ADD COLUMN leaderboard_name text
        CHECK (
            leaderboard_name IS NULL
            OR (
                char_length(btrim(leaderboard_name)) BETWEEN 2 AND 20
                AND leaderboard_name = btrim(leaderboard_name)
            )
        ),
    ADD COLUMN leaderboard_published_at timestamptz;

-- The original schema allowed exactly one run per installation+campaign. Keep
-- legacy rows addressable through attempt_id='legacy', while new clients create
-- independent attempts and can therefore improve their best time.
ALTER TABLE synesthesia_runs
    DROP CONSTRAINT IF EXISTS synesthesia_runs_workspace_id_campaign_slug_install_hash_key;

CREATE UNIQUE INDEX synesthesia_runs_attempt_uidx
    ON synesthesia_runs (workspace_id, campaign_slug, install_hash, attempt_id);

-- Supports DISTINCT ON (install_hash) best-attempt selection and the subsequent
-- global time ordering without exposing the pseudonymous install hash publicly.
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

CREATE INDEX synesthesia_runs_leaderboard_public_idx
    ON synesthesia_runs (
        workspace_id,
        campaign_slug,
        client_total_elapsed_ms,
        completed_at,
        id
    )
    WHERE completed_at IS NOT NULL
      AND client_total_elapsed_ms IS NOT NULL
      AND leaderboard_name IS NOT NULL;
