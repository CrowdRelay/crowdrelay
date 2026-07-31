-- Secure, revocable concert QR campaigns and venue check-ins.
-- Public QR payloads are HMAC signed by the API; only campaign identifiers and
-- bounded expiry metadata are encoded. Durable campaign state remains the
-- authority for activation, revocation, time windows and capacity.

CREATE TABLE concert_qr_campaigns (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    label text NOT NULL CHECK (btrim(label) <> '' AND char_length(label) <= 160),
    valid_from timestamptz NOT NULL,
    valid_until timestamptz NOT NULL,
    max_checkins integer CHECK (max_checkins IS NULL OR max_checkins BETWEEN 1 AND 1000000),
    active boolean NOT NULL DEFAULT true,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, id, event_id),
    CONSTRAINT concert_qr_campaigns_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (valid_until > valid_from),
    CHECK (valid_until <= valid_from + interval '14 days'),
    CHECK (revoked_at IS NULL OR revoked_at >= created_at),
    CHECK (active OR revoked_at IS NOT NULL)
);

CREATE TRIGGER concert_qr_campaigns_set_updated_at
BEFORE UPDATE ON concert_qr_campaigns
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX concert_qr_campaigns_event_idx
    ON concert_qr_campaigns (workspace_id, event_id, valid_from DESC, id DESC);
CREATE INDEX concert_qr_campaigns_active_idx
    ON concert_qr_campaigns (workspace_id, valid_until, id)
    WHERE active AND revoked_at IS NULL;

CREATE TABLE concert_checkins (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    campaign_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    checked_in_at timestamptz NOT NULL DEFAULT now(),
    request_id text,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, event_id, fan_id),
    CONSTRAINT concert_checkins_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT concert_checkins_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id, event_id)
        REFERENCES concert_qr_campaigns (workspace_id, id, event_id)
        ON DELETE RESTRICT,
    CONSTRAINT concert_checkins_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX concert_checkins_campaign_time_idx
    ON concert_checkins (workspace_id, campaign_id, checked_in_at, id);
CREATE INDEX concert_checkins_fan_time_idx
    ON concert_checkins (workspace_id, fan_id, checked_in_at DESC, id DESC);

ALTER TABLE reward_draws
    ADD COLUMN entries_per_checkin integer NOT NULL DEFAULT 0
        CHECK (entries_per_checkin BETWEEN 0 AND 100000);

ALTER TABLE reward_draw_candidates
    ADD COLUMN concert_checkins integer NOT NULL DEFAULT 0
        CHECK (concert_checkins >= 0),
    ADD COLUMN checkin_entries integer NOT NULL DEFAULT 0
        CHECK (checkin_entries >= 0);
