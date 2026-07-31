-- Phase 2: referral qualification state, deterministic merch rewards,
-- privacy-safe fan sessions and atomic coupon redemption.

ALTER TABLE referral_attributions
    ADD COLUMN status text NOT NULL DEFAULT 'qualified'
        CHECK (status IN ('pending', 'qualified', 'rejected', 'reversed')),
    ADD COLUMN qualification_reason text,
    ADD COLUMN qualified_at timestamptz,
    ADD COLUMN rejected_at timestamptz,
    ADD COLUMN reversed_at timestamptz;

UPDATE referral_attributions
SET qualified_at = accepted_at
WHERE status = 'qualified' AND qualified_at IS NULL;

ALTER TABLE referral_attributions
    ADD CONSTRAINT referral_attributions_state_timestamps_check CHECK (
        (status = 'pending'
            AND qualified_at IS NULL
            AND rejected_at IS NULL
            AND reversed_at IS NULL)
        OR (status = 'qualified'
            AND qualified_at IS NOT NULL
            AND rejected_at IS NULL
            AND reversed_at IS NULL)
        OR (status = 'rejected'
            AND rejected_at IS NOT NULL
            AND reversed_at IS NULL)
        OR (status = 'reversed'
            AND qualified_at IS NOT NULL
            AND reversed_at IS NOT NULL)
    );

CREATE INDEX referral_attributions_referrer_status_idx
    ON referral_attributions (workspace_id, referrer_fan_id, status, qualified_at DESC, id);

CREATE TABLE fan_sessions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    session_token_hash bytea NOT NULL UNIQUE
        CHECK (octet_length(session_token_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    UNIQUE (workspace_id, id),
    CONSTRAINT fan_sessions_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (expires_at > created_at),
    CHECK (last_seen_at >= created_at),
    CHECK (revoked_at IS NULL OR revoked_at >= created_at)
);

CREATE INDEX fan_sessions_active_fan_idx
    ON fan_sessions (workspace_id, fan_id, expires_at DESC)
    WHERE revoked_at IS NULL;
CREATE INDEX fan_sessions_expiry_idx ON fan_sessions (expires_at);

ALTER TABLE reward_rules
    ADD COLUMN version integer NOT NULL DEFAULT 1 CHECK (version > 0);

CREATE UNIQUE INDEX reward_rules_workspace_name_idx
    ON reward_rules (workspace_id, name);
CREATE INDEX reward_rules_referral_threshold_idx
    ON reward_rules (workspace_id, reward_type, threshold, id)
    WHERE active;

ALTER TABLE reward_grants
    ADD COLUMN issued_at timestamptz,
    ADD COLUMN delivered_at timestamptz,
    ADD COLUMN redeemed_at timestamptz,
    ADD COLUMN expires_at timestamptz,
    ADD COLUMN revoked_at timestamptz;

ALTER TABLE reward_grants
    ADD CONSTRAINT reward_grants_timestamps_check CHECK (
        (issued_at IS NULL OR issued_at >= created_at)
        AND (delivered_at IS NULL OR delivered_at >= created_at)
        AND (redeemed_at IS NULL OR redeemed_at >= created_at)
        AND (expires_at IS NULL OR expires_at > created_at)
        AND (revoked_at IS NULL OR revoked_at >= created_at)
    );

ALTER TABLE merch_coupons
    ADD COLUMN status text NOT NULL DEFAULT 'issued',
    ADD COLUMN redeemed_at timestamptz,
    ADD COLUMN revoked_at timestamptz,
    ADD COLUMN last_order_reference text;

ALTER TABLE merch_coupons
    ADD CONSTRAINT merch_coupons_status_check CHECK (
        (status = 'issued' AND redeemed_at IS NULL AND revoked_at IS NULL)
        OR (status = 'redeemed' AND redeemed_at IS NOT NULL AND revoked_at IS NULL)
        OR (status = 'expired' AND revoked_at IS NULL)
        OR (status = 'revoked' AND revoked_at IS NOT NULL)
    ),
    ADD CONSTRAINT merch_coupons_order_reference_check CHECK (
        last_order_reference IS NULL
        OR (btrim(last_order_reference) <> '' AND char_length(last_order_reference) <= 128)
    );

CREATE INDEX merch_coupons_active_expiry_idx
    ON merch_coupons (workspace_id, expires_at, id)
    WHERE status = 'issued';

CREATE TABLE coupon_redemptions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    coupon_id uuid NOT NULL,
    order_reference text NOT NULL
        CHECK (btrim(order_reference) <> '' AND char_length(order_reference) <= 128),
    usage_number integer NOT NULL CHECK (usage_number > 0),
    redeemed_at timestamptz NOT NULL DEFAULT now(),
    request_id text NOT NULL
        CHECK (btrim(request_id) <> '' AND char_length(request_id) <= 128),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, coupon_id, order_reference),
    UNIQUE (workspace_id, coupon_id, usage_number),
    CONSTRAINT coupon_redemptions_coupon_fk
        FOREIGN KEY (workspace_id, coupon_id)
        REFERENCES merch_coupons (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX coupon_redemptions_time_idx
    ON coupon_redemptions (workspace_id, redeemed_at DESC, id);
