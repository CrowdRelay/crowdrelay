-- Compatibility recovery for clients affected by the historical room-timing bug.
--
-- `recovery_completed_at` deliberately does NOT carry a fabricated elapsed time.
-- It may unlock My Signal linking and the completion reward path for a locally
-- completed 11-room save, but competitive leaderboard queries continue to use
-- only the normal `completed_at + client_total_elapsed_ms` pair.
ALTER TABLE synesthesia_runs
    ADD COLUMN recovery_completed_at timestamptz;

CREATE INDEX synesthesia_runs_recovery_completion_idx
    ON synesthesia_runs (workspace_id, campaign_slug, recovery_completed_at, id)
    WHERE recovery_completed_at IS NOT NULL;
