-- The show ladder approval (P.4): one durable "yes" over a night's whole
-- announce-to-recap ladder instead of one approval per lever. Approving the
-- ladder releases the rungs already parked in `awaiting_approval` and
-- pre-authorizes the ones not yet decided — the domain still measures each
-- lever's own evidence gates at decision time, so a denied rung stays denied.
--
-- Revocation stops future pre-authorization and cancels the rungs the ladder
-- itself released but that have not executed yet; a rung the operator approved
-- individually keeps its own approval and still fires.
CREATE TABLE viryaos_show_ladder_approvals (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    approved_by text NOT NULL,
    approved_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz,
    revoked_by text,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT show_ladder_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE
);

-- One live approval per show. A revoke closes the row rather than deleting it,
-- so the ledger can always answer "who approved this ladder and when did it
-- stop applying".
CREATE UNIQUE INDEX viryaos_show_ladder_approvals_live_idx
    ON viryaos_show_ladder_approvals (workspace_id, event_id)
    WHERE revoked_at IS NULL;
