-- Durable, tenant-safe acquisition attribution for Phase 1 fan signups.
-- Referral input is recorded as context only; promotion into accepted
-- referral_attributions belongs to the separately reviewed Phase 2 policy.

CREATE UNIQUE INDEX referral_codes_one_active_per_fan_idx
    ON referral_codes (workspace_id, fan_id)
    WHERE active;

CREATE TABLE fan_acquisition_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    campaign_id uuid,
    anonymous_visitor_id uuid,
    source text NOT NULL
        CHECK (btrim(source) <> '' AND char_length(source) <= 128),
    request_id text NOT NULL
        CHECK (btrim(request_id) <> '' AND char_length(request_id) <= 128),
    referral_code_id uuid,
    referrer_fan_id uuid,
    occurred_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    CONSTRAINT fan_acquisition_events_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT fan_acquisition_events_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES campaigns (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT fan_acquisition_events_referral_code_owner_fk
        FOREIGN KEY (workspace_id, referral_code_id, referrer_fan_id)
        REFERENCES referral_codes (workspace_id, id, fan_id)
        ON DELETE RESTRICT,
    CHECK (
        (referral_code_id IS NULL AND referrer_fan_id IS NULL)
        OR (referral_code_id IS NOT NULL AND referrer_fan_id IS NOT NULL)
    ),
    CHECK (referrer_fan_id IS NULL OR referrer_fan_id <> fan_id)
);

CREATE INDEX fan_acquisition_events_fan_time_idx
    ON fan_acquisition_events (workspace_id, fan_id, occurred_at DESC, id);

CREATE INDEX fan_acquisition_events_campaign_time_idx
    ON fan_acquisition_events (workspace_id, campaign_id, occurred_at DESC, id)
    WHERE campaign_id IS NOT NULL;

CREATE INDEX fan_acquisition_events_visitor_time_idx
    ON fan_acquisition_events (
        workspace_id,
        anonymous_visitor_id,
        occurred_at DESC,
        id
    )
    WHERE anonymous_visitor_id IS NOT NULL;

-- Attribution history is immutable, but deletion remains available to a
-- separately authorized privacy-erasure workflow.
CREATE TRIGGER fan_acquisition_events_no_update
BEFORE UPDATE ON fan_acquisition_events
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_append_only_mutation();
