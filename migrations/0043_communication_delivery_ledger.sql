-- Durable at-most-once handoff for communication campaign provider delivery.
--
-- n8n remains a thin provider adapter: CrowdRelay snapshots recipients and owns
-- the claim/result state. A recipient must be claimed before an external send.
-- Claims that become ambiguous are failed closed instead of being automatically
-- retried, because Gmail does not expose a request idempotency primitive.

CREATE TABLE communication_campaign_deliveries (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    campaign_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    attempt_key text NOT NULL
        CHECK (btrim(attempt_key) <> '' AND char_length(attempt_key) <= 160),
    status text NOT NULL
        CHECK (status IN ('claimed', 'delivered', 'failed')),
    provider_reference text
        CHECK (provider_reference IS NULL OR char_length(provider_reference) <= 240),
    error_code text
        CHECK (error_code IS NULL OR char_length(error_code) <= 120),
    claimed_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, campaign_id, fan_id),
    CONSTRAINT communication_campaign_deliveries_recipient_fk
        FOREIGN KEY (workspace_id, campaign_id, fan_id)
        REFERENCES communication_campaign_recipients (workspace_id, campaign_id, fan_id)
        ON DELETE CASCADE,
    CHECK (
        (status = 'claimed' AND completed_at IS NULL)
        OR (status IN ('delivered', 'failed') AND completed_at IS NOT NULL)
    )
);

CREATE UNIQUE INDEX communication_campaign_deliveries_attempt_key_idx
    ON communication_campaign_deliveries (workspace_id, campaign_id, attempt_key);

CREATE INDEX communication_campaign_deliveries_status_idx
    ON communication_campaign_deliveries (workspace_id, campaign_id, status, updated_at, fan_id);
