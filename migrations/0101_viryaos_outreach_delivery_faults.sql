-- Sending like somebody who wants replies.
--
-- Deliverability is not a detail. A burned sending domain does not degrade the
-- outreach channel, it ends it, and it takes the transactional mail sharing
-- that domain with it. The band cannot buy its reputation back.
--
-- Bounces and complaints have been invisible to the agent until now, which
-- means the one number that should stop a wave was the one number nothing
-- recorded. This is that record: one row per fault, so a rate can be computed
-- over a window rather than inferred from a counter somebody has to reset.
--
-- A hard bounce is also a fact about one address, and suppression already has a
-- home — `do_not_contact` on the target since 0034. Nothing new is invented for
-- it here.

CREATE TABLE viryaos_outreach_delivery_faults (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    target_id uuid NOT NULL,
    fault text NOT NULL CHECK (fault IN ('hard_bounce', 'soft_bounce', 'complaint')),
    -- The provider's own reference, where it gave one. Kept so a disputed
    -- complaint can be traced back to the message that caused it rather than
    -- argued about from a count.
    provider_reference text
        CHECK (provider_reference IS NULL OR (
            btrim(provider_reference) <> '' AND char_length(provider_reference) <= 200
        )),
    occurred_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT viryaos_outreach_delivery_faults_target_fk
        FOREIGN KEY (workspace_id, target_id)
        REFERENCES viryaos_outreach_targets (workspace_id, id)
        ON DELETE CASCADE,
    -- One report per provider reference. Webhooks retry, and a retried
    -- complaint counted twice is a halt nobody earned.
    UNIQUE (workspace_id, provider_reference)
);

-- The rate is read over a rolling window on every cycle that wants to send.
CREATE INDEX viryaos_outreach_delivery_faults_window_idx
    ON viryaos_outreach_delivery_faults (workspace_id, occurred_at DESC);

-- When this workspace first sent to somebody it does not know. The ramp starts
-- here: a standing start at the operator's full weekly budget reads to a
-- receiving provider as exactly what it looks like.
ALTER TABLE workspaces
    ADD COLUMN first_third_party_send_at timestamptz;
