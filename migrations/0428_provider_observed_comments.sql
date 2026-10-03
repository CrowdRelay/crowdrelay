-- A prospect may only be discovered from a comment actually observed at a
-- provider boundary.
--
-- community_comments is also convenient fixture/manual storage. Treating mere
-- row existence as proof of a real external person lets synthetic or imported
-- rows enter FAN SCOUT and eventually the acquisition scoreboard. The workers
-- below stamp provider_observed_at only after a successful provider read has
-- returned that exact platform_comment_id.
--
-- Existing rows are deliberately NOT backfilled. Their provenance predates
-- this receipt and cannot be reconstructed without guessing.

ALTER TABLE community_comments
    ADD COLUMN IF NOT EXISTS provider_observed_at timestamptz;

COMMENT ON COLUMN community_comments.provider_observed_at IS
    'When the exact platform_comment_id was returned by the external provider. NULL is unverified storage and must not create a FAN SCOUT prospect.';

CREATE INDEX IF NOT EXISTS community_comments_provider_observed_idx
    ON community_comments (workspace_id, provider_observed_at DESC)
    WHERE provider_observed_at IS NOT NULL;
