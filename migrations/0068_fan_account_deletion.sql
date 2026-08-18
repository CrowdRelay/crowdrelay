-- Fan account deletion state.
--
-- We cannot hard-delete `fans`: consent evidence is append-only and intentionally
-- uses ON DELETE RESTRICT. Account deletion therefore erases direct identity and
-- authentication material while retaining a pseudonymous tombstone for consent,
-- paid-ticket and audit integrity.

ALTER TABLE fans
    ADD COLUMN deleted_at timestamptz;

ALTER TABLE fans
    ADD CONSTRAINT fans_deleted_state_check CHECK (
        deleted_at IS NULL
        OR (
            status = 'suppressed'
            AND display_name IS NULL
            AND locale IS NULL
            AND normalized_email ~ '^deleted-[0-9a-f-]{36}@account[.]invalid$'
        )
    );

CREATE INDEX fans_deleted_at_idx
    ON fans (workspace_id, deleted_at, id)
    WHERE deleted_at IS NOT NULL;
