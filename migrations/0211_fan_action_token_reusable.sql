-- Reusable fan action tokens, for app-store review access.
--
-- Every fan action token is single-use: the confirmation path sets
-- `consumed_at` and refuses a token that already has one. That is right for a
-- token mailed to a person, and wrong for the credential handed to a Google
-- Play reviewer. Review is not one tap by one person — it is several
-- reviewers, retries, and a fresh review on every update — so a single-use
-- demo login works once and then reports a conflict to everyone after,
-- including the operator who tested it before submitting.
--
-- Scope is deliberately narrow:
--
--   * Only `purpose = 'session'`. A session token acts on a fan who is already
--     active, which is a state that survives repeated use. `confirm` is bound
--     to the pending-to-active transition and must stay single-use: the rule
--     that a stale confirmation can never reactivate an unsubscribed fan is a
--     real protection, not an obstacle, and this change does not touch it.
--
--   * Expiry still applies, and a reusable token may not outlive 180 days.
--     A reusable credential that never expires is a permanent unauthenticated
--     door into a fan account; the horizon keeps it a review artefact that
--     lapses on its own if nobody remembers to revoke it.
--
--   * Default false. Marking a token reusable is a deliberate UPDATE against
--     one row, never something a code path decides.
ALTER TABLE fan_action_tokens
    ADD COLUMN IF NOT EXISTS reusable boolean NOT NULL DEFAULT false;

ALTER TABLE fan_action_tokens
    DROP CONSTRAINT IF EXISTS fan_action_tokens_reusable_purpose_check;
ALTER TABLE fan_action_tokens
    ADD CONSTRAINT fan_action_tokens_reusable_purpose_check
    CHECK (NOT reusable OR purpose = 'session');

ALTER TABLE fan_action_tokens
    DROP CONSTRAINT IF EXISTS fan_action_tokens_reusable_horizon_check;
ALTER TABLE fan_action_tokens
    ADD CONSTRAINT fan_action_tokens_reusable_horizon_check
    CHECK (NOT reusable OR expires_at <= created_at + interval '180 days');

-- Reusable tokens are rare and worth finding quickly when auditing who can
-- reach a fan account without a mailed link.
CREATE INDEX IF NOT EXISTS fan_action_tokens_reusable_idx
    ON fan_action_tokens (workspace_id, expires_at)
    WHERE reusable;
