-- V4 fan context + Synesthesia identity handoff.
--
-- Additive only. Existing fan, mail, ticket and draw flows keep their contracts.
-- A completed anonymous Synesthesia run may be linked to an existing fan through
-- a short-lived, single-fan handoff secret. The raw secret is never persisted.

ALTER TABLE synesthesia_runs
    ADD COLUMN fan_id uuid,
    ADD COLUMN linked_at timestamptz,
    ADD COLUMN handoff_token_hash bytea,
    ADD COLUMN handoff_expires_at timestamptz,
    ADD CONSTRAINT synesthesia_runs_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE RESTRICT,
    ADD CONSTRAINT synesthesia_runs_handoff_hash_check
        CHECK (handoff_token_hash IS NULL OR octet_length(handoff_token_hash) = 32),
    ADD CONSTRAINT synesthesia_runs_link_check
        CHECK ((fan_id IS NULL) = (linked_at IS NULL)),
    ADD CONSTRAINT synesthesia_runs_handoff_expiry_check
        CHECK ((handoff_token_hash IS NULL) = (handoff_expires_at IS NULL));

CREATE UNIQUE INDEX synesthesia_runs_live_handoff_uidx
    ON synesthesia_runs (workspace_id, handoff_token_hash)
    WHERE handoff_token_hash IS NOT NULL;

CREATE INDEX synesthesia_runs_fan_completion_idx
    ON synesthesia_runs (workspace_id, fan_id, completed_at DESC, id DESC)
    WHERE fan_id IS NOT NULL;
