-- Provider-side execution claims and durable team handoff identity.
--
-- External providers such as Gmail/Discord/Drive do not all expose a safe
-- request-idempotency primitive. A claim is therefore acquired before the
-- provider call. A crash after provider acceptance but before the terminal
-- receipt leaves the claim ambiguous and fail-closed instead of replaying the
-- side effect. Explicit provider failures may be retried with a fresh token.

ALTER TABLE viryaos_team_assignments
    ADD COLUMN source_ref text CHECK (
        source_ref IS NULL OR (btrim(source_ref) <> '' AND char_length(source_ref) <= 120)
    );

CREATE UNIQUE INDEX viryaos_team_assignments_source_identity_uidx
    ON viryaos_team_assignments (workspace_id, source_kind, source_id, source_ref)
    WHERE source_ref IS NOT NULL;

CREATE TABLE viryaos_autopilot_execution_claims (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    action_id uuid NOT NULL,
    executor_id text NOT NULL CHECK (btrim(executor_id) <> '' AND char_length(executor_id) <= 120),
    claim_token uuid NOT NULL,
    status text NOT NULL CHECK (status IN ('claimed','succeeded','failed')),
    attempt_number integer NOT NULL DEFAULT 1 CHECK (attempt_number BETWEEN 1 AND 100),
    provider_reference text CHECK (
        provider_reference IS NULL OR char_length(provider_reference) <= 240
    ),
    error_kind text CHECK (error_kind IS NULL OR char_length(error_kind) <= 96),
    claimed_at timestamptz NOT NULL,
    completed_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, action_id, executor_id),
    CONSTRAINT viryaos_autopilot_execution_claims_action_fk
        FOREIGN KEY (workspace_id, action_id)
        REFERENCES viryaos_autopilot_actions (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (
        (status = 'claimed' AND completed_at IS NULL)
        OR (status IN ('succeeded','failed') AND completed_at IS NOT NULL)
    )
);
CREATE TRIGGER viryaos_autopilot_execution_claims_set_updated_at
BEFORE UPDATE ON viryaos_autopilot_execution_claims
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();
CREATE INDEX viryaos_autopilot_execution_claims_status_idx
    ON viryaos_autopilot_execution_claims (workspace_id, status, claimed_at, action_id);
