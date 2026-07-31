-- Initial production schema for CrowdRelay.
-- Every tenant-owned row carries workspace_id, and every relationship between
-- tenant-owned rows enforces that both rows belong to the same workspace.
--
-- Random-draw tables are intentionally absent. They may only be introduced by
-- a later migration after the legal/compliance gate has been resolved.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE FUNCTION crowdrelay_set_updated_at()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END;
$$;

CREATE FUNCTION crowdrelay_reject_append_only_mutation()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    RAISE EXCEPTION '% is append-only; % is not allowed',
        TG_TABLE_SCHEMA || '.' || TG_TABLE_NAME,
        TG_OP
        USING ERRCODE = '55000';
END;
$$;

CREATE TABLE workspaces (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug text NOT NULL UNIQUE
        CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    name text NOT NULL CHECK (btrim(name) <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER workspaces_set_updated_at
BEFORE UPDATE ON workspaces
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE cities (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    name text NOT NULL CHECK (btrim(name) <> ''),
    country_code char(2) NOT NULL CHECK (country_code ~ '^[A-Z]{2}$'),
    region text,
    latitude double precision,
    longitude double precision,
    UNIQUE (country_code, slug),
    CHECK (
        (latitude IS NULL AND longitude IS NULL)
        OR (
            latitude IS NOT NULL
            AND longitude IS NOT NULL
            AND latitude BETWEEN -90.0 AND 90.0
            AND longitude BETWEEN -180.0 AND 180.0
        )
    )
);

CREATE TABLE workspace_members (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    normalized_email text NOT NULL CHECK (btrim(normalized_email) <> ''),
    display_name text,
    role text NOT NULL CHECK (role IN ('owner', 'admin', 'staff')),
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('invited', 'active', 'disabled')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, normalized_email)
);

CREATE TRIGGER workspace_members_set_updated_at
BEFORE UPDATE ON workspace_members
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE workspace_member_sessions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    member_id uuid NOT NULL,
    session_token_hash bytea NOT NULL UNIQUE
        CHECK (octet_length(session_token_hash) = 32),
    csrf_token_hash bytea NOT NULL
        CHECK (octet_length(csrf_token_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, id, member_id),
    CONSTRAINT workspace_member_sessions_member_fk
        FOREIGN KEY (workspace_id, member_id)
        REFERENCES workspace_members (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (expires_at > created_at),
    CHECK (last_seen_at >= created_at),
    CHECK (revoked_at IS NULL OR revoked_at >= created_at)
);

CREATE TABLE fans (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    normalized_email text NOT NULL CHECK (btrim(normalized_email) <> ''),
    display_name text,
    locale text,
    status text NOT NULL CHECK (status IN ('pending','active','unsubscribed','suppressed')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, normalized_email)
);

CREATE TRIGGER fans_set_updated_at
BEFORE UPDATE ON fans
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE fan_consents (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    fan_id uuid NOT NULL,
    purpose text NOT NULL CHECK (btrim(purpose) <> ''),
    granted boolean NOT NULL,
    policy_version text NOT NULL CHECK (btrim(policy_version) <> ''),
    source text NOT NULL CHECK (btrim(source) <> ''),
    request_id text,
    recorded_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    CONSTRAINT fan_consents_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE TRIGGER fan_consents_append_only
BEFORE UPDATE OR DELETE ON fan_consents
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_append_only_mutation();

CREATE TABLE fan_city_interests (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    city_id uuid NOT NULL REFERENCES cities(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, fan_id, city_id),
    CONSTRAINT fan_city_interests_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);

CREATE TABLE city_aggregates (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    city_id uuid NOT NULL REFERENCES cities(id),
    confirmed_fan_count bigint NOT NULL DEFAULT 0 CHECK (confirmed_fan_count >= 0),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, city_id)
);

CREATE TABLE campaigns (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (btrim(name) <> ''),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);

CREATE TRIGGER campaigns_set_updated_at
BEFORE UPDATE ON campaigns
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE smart_links (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    campaign_id uuid,
    slug text NOT NULL CHECK (slug ~ '^[a-zA-Z0-9][a-zA-Z0-9_-]{0,127}$'),
    destination_url text NOT NULL
        CHECK (destination_url ~* '^https?://'),
    active boolean NOT NULL DEFAULT true,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug),
    CONSTRAINT smart_links_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES campaigns (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE FUNCTION crowdrelay_touch_smart_link()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    NEW.version := OLD.version + 1;
    NEW.updated_at := clock_timestamp();
    RETURN NEW;
END;
$$;

CREATE TRIGGER smart_links_touch
BEFORE UPDATE ON smart_links
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_touch_smart_link();

CREATE TABLE click_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    smart_link_id uuid NOT NULL,
    campaign_id uuid,
    anonymous_visitor_id uuid,
    referrer_host text,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT click_events_smart_link_fk
        FOREIGN KEY (workspace_id, smart_link_id)
        REFERENCES smart_links (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT click_events_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES campaigns (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX click_events_link_time_idx
    ON click_events (workspace_id, smart_link_id, occurred_at DESC);
CREATE INDEX click_events_retention_idx
    ON click_events (occurred_at);

CREATE TABLE referral_codes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    code text NOT NULL CHECK (code ~ '^[A-Za-z0-9_-]{6,128}$'),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, id, fan_id),
    UNIQUE (workspace_id, code),
    CONSTRAINT referral_codes_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE
);

CREATE TABLE referral_attributions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    referrer_fan_id uuid NOT NULL,
    referred_fan_id uuid NOT NULL,
    referral_code_id uuid NOT NULL,
    accepted_at timestamptz NOT NULL DEFAULT now(),
    CHECK (referrer_fan_id <> referred_fan_id),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, referred_fan_id),
    CONSTRAINT referral_attributions_referrer_fk
        FOREIGN KEY (workspace_id, referrer_fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT referral_attributions_referred_fk
        FOREIGN KEY (workspace_id, referred_fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT referral_attributions_code_owner_fk
        FOREIGN KEY (workspace_id, referral_code_id, referrer_fan_id)
        REFERENCES referral_codes (workspace_id, id, fan_id)
        ON DELETE RESTRICT
);

CREATE TABLE events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    city_id uuid REFERENCES cities(id),
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    title text NOT NULL CHECK (btrim(title) <> ''),
    venue text,
    starts_at timestamptz NOT NULL,
    doors_at timestamptz,
    ticket_url text CHECK (ticket_url IS NULL OR ticket_url ~* '^https?://'),
    status text NOT NULL CHECK (status IN ('draft','published','cancelled','completed')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug),
    CHECK (doors_at IS NULL OR doors_at <= starts_at)
);

CREATE TRIGGER events_set_updated_at
BEFORE UPDATE ON events
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE event_interests (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    campaign_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, event_id, fan_id),
    CONSTRAINT event_interests_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT event_interests_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT event_interests_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES campaigns (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE TABLE reward_rules (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (btrim(name) <> ''),
    reward_type text NOT NULL CHECK (btrim(reward_type) <> ''),
    threshold integer CHECK (threshold IS NULL OR threshold > 0),
    config jsonb NOT NULL DEFAULT '{}'::jsonb,
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id)
);

CREATE TRIGGER reward_rules_set_updated_at
BEFORE UPDATE ON reward_rules
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE reward_grants (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    fan_id uuid NOT NULL,
    reward_rule_id uuid NOT NULL,
    qualification_key text NOT NULL CHECK (btrim(qualification_key) <> ''),
    status text NOT NULL CHECK (status IN ('pending','issued','delivered','redeemed','expired','revoked')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, reward_rule_id, fan_id, qualification_key),
    CONSTRAINT reward_grants_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_grants_rule_fk
        FOREIGN KEY (workspace_id, reward_rule_id)
        REFERENCES reward_rules (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE TRIGGER reward_grants_set_updated_at
BEFORE UPDATE ON reward_grants
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE merch_coupons (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    reward_grant_id uuid NOT NULL,
    code_hash bytea NOT NULL UNIQUE CHECK (octet_length(code_hash) >= 32),
    code_display text NOT NULL CHECK (btrim(code_display) <> ''),
    discount_percent numeric(5,2)
        CHECK (discount_percent IS NULL OR discount_percent > 0 AND discount_percent <= 100),
    max_uses integer NOT NULL DEFAULT 1 CHECK (max_uses > 0),
    used_count integer NOT NULL DEFAULT 0 CHECK (used_count >= 0),
    expires_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, reward_grant_id),
    CONSTRAINT merch_coupons_reward_grant_fk
        FOREIGN KEY (workspace_id, reward_grant_id)
        REFERENCES reward_grants (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (used_count <= max_uses)
);

CREATE TABLE admission_pools (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    name text NOT NULL CHECK (btrim(name) <> ''),
    capacity integer NOT NULL CHECK (capacity >= 0),
    issued_count integer NOT NULL DEFAULT 0 CHECK (issued_count >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, id, event_id),
    CONSTRAINT admission_pools_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (issued_count <= capacity)
);

CREATE TRIGGER admission_pools_set_updated_at
BEFORE UPDATE ON admission_pools
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE admission_passes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    admission_pool_id uuid NOT NULL,
    fan_id uuid NOT NULL,
    issued_by_member_id uuid,
    issuance_method text NOT NULL
        CHECK (issuance_method IN ('manual', 'deterministic_reward', 'first_come')),
    public_reference text NOT NULL UNIQUE CHECK (btrim(public_reference) <> ''),
    claim_token_hash bytea UNIQUE
        CHECK (claim_token_hash IS NULL OR octet_length(claim_token_hash) = 32),
    claim_expires_at timestamptz NOT NULL,
    claim_token_consumed_at timestamptz,
    status text NOT NULL CHECK (status IN ('issued','claimed','redeemed','revoked','expired')),
    issued_at timestamptz NOT NULL DEFAULT now(),
    claimed_at timestamptz,
    redeemed_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, admission_pool_id, fan_id),
    CONSTRAINT admission_passes_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT admission_passes_pool_event_fk
        FOREIGN KEY (workspace_id, admission_pool_id, event_id)
        REFERENCES admission_pools (workspace_id, id, event_id)
        ON DELETE CASCADE,
    CONSTRAINT admission_passes_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT admission_passes_issuer_fk
        FOREIGN KEY (workspace_id, issued_by_member_id)
        REFERENCES workspace_members (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (issuance_method <> 'manual' OR issued_by_member_id IS NOT NULL),
    CHECK (claim_expires_at > issued_at),
    CHECK (claim_token_consumed_at IS NULL OR claim_token_consumed_at >= issued_at),
    CHECK (claimed_at IS NULL OR claimed_at >= issued_at),
    CHECK (redeemed_at IS NULL OR redeemed_at >= issued_at),
    CHECK (
        status <> 'issued'
        OR (
            claim_token_hash IS NOT NULL
            AND claim_token_consumed_at IS NULL
            AND claimed_at IS NULL
            AND redeemed_at IS NULL
        )
    ),
    CHECK (
        status NOT IN ('claimed', 'redeemed')
        OR (
            claim_token_hash IS NULL
            AND claim_token_consumed_at IS NOT NULL
        )
    ),
    CHECK (status <> 'claimed' OR claimed_at IS NOT NULL),
    CHECK (status <> 'redeemed' OR redeemed_at IS NOT NULL)
);

CREATE TRIGGER admission_passes_set_updated_at
BEFORE UPDATE ON admission_passes
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE pass_sessions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    pass_id uuid NOT NULL,
    session_token_hash bytea NOT NULL UNIQUE
        CHECK (octet_length(session_token_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    revoked_at timestamptz,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, pass_id),
    CONSTRAINT pass_sessions_pass_fk
        FOREIGN KEY (workspace_id, pass_id)
        REFERENCES admission_passes (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (expires_at > created_at),
    CHECK (last_seen_at >= created_at),
    CHECK (revoked_at IS NULL OR revoked_at >= created_at)
);

CREATE TABLE pass_redemptions (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    pass_id uuid NOT NULL,
    staff_member_id uuid NOT NULL,
    staff_session_id uuid NOT NULL,
    redeemed_at timestamptz NOT NULL DEFAULT now(),
    request_id text NOT NULL CHECK (btrim(request_id) <> ''),
    result_metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, pass_id),
    UNIQUE (workspace_id, request_id),
    CONSTRAINT pass_redemptions_pass_fk
        FOREIGN KEY (workspace_id, pass_id)
        REFERENCES admission_passes (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT pass_redemptions_member_fk
        FOREIGN KEY (workspace_id, staff_member_id)
        REFERENCES workspace_members (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT pass_redemptions_session_member_fk
        FOREIGN KEY (workspace_id, staff_session_id, staff_member_id)
        REFERENCES workspace_member_sessions (workspace_id, id, member_id)
        ON DELETE RESTRICT
);

CREATE TABLE audit_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    actor_kind text NOT NULL CHECK (actor_kind IN ('member', 'system', 'service')),
    actor_member_id uuid,
    action text NOT NULL CHECK (btrim(action) <> ''),
    target_type text NOT NULL CHECK (btrim(target_type) <> ''),
    target_id text,
    request_id text,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    CONSTRAINT audit_events_actor_member_fk
        FOREIGN KEY (workspace_id, actor_member_id)
        REFERENCES workspace_members (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (
        (actor_kind = 'member' AND actor_member_id IS NOT NULL)
        OR (actor_kind <> 'member' AND actor_member_id IS NULL)
    )
);

CREATE INDEX audit_events_workspace_time_idx
    ON audit_events (workspace_id, occurred_at DESC, id);

CREATE TRIGGER audit_events_append_only
BEFORE UPDATE OR DELETE ON audit_events
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_append_only_mutation();

CREATE TABLE outbox_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    event_type text NOT NULL CHECK (btrim(event_type) <> ''),
    event_version integer NOT NULL DEFAULT 1 CHECK (event_version > 0),
    payload jsonb NOT NULL,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'processing', 'delivered', 'dead')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts integer NOT NULL DEFAULT 12 CHECK (max_attempts > 0),
    available_at timestamptz NOT NULL DEFAULT now(),
    locked_at timestamptz,
    lock_owner text,
    lease_expires_at timestamptz,
    last_error_kind text,
    request_id text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz,
    dead_at timestamptz,
    UNIQUE (workspace_id, id),
    CHECK (attempts <= max_attempts),
    CHECK (
        (
            status = 'processing'
            AND locked_at IS NOT NULL
            AND lock_owner IS NOT NULL
            AND btrim(lock_owner) <> ''
            AND lease_expires_at IS NOT NULL
            AND lease_expires_at > locked_at
        )
        OR (
            status <> 'processing'
            AND locked_at IS NULL
            AND lock_owner IS NULL
            AND lease_expires_at IS NULL
        )
    ),
    CHECK ((status = 'delivered') = (delivered_at IS NOT NULL)),
    CHECK ((status = 'dead') = (dead_at IS NOT NULL))
);

CREATE TRIGGER outbox_events_set_updated_at
BEFORE UPDATE ON outbox_events
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX outbox_claim_idx
    ON outbox_events (status, available_at, id);
CREATE INDEX outbox_lease_recovery_idx
    ON outbox_events (lease_expires_at, id)
    WHERE status = 'processing';

CREATE TABLE webhook_endpoints (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (btrim(name) <> ''),
    url text NOT NULL CHECK (url ~* '^https?://'),
    signing_secret_ref text NOT NULL CHECK (btrim(signing_secret_ref) <> ''),
    signing_secret_version integer NOT NULL DEFAULT 1
        CHECK (signing_secret_version > 0),
    previous_signing_secret_ref text,
    previous_secret_valid_until timestamptz,
    timeout_ms integer NOT NULL DEFAULT 10000
        CHECK (timeout_ms BETWEEN 100 AND 60000),
    max_attempts integer NOT NULL DEFAULT 12
        CHECK (max_attempts BETWEEN 1 AND 100),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, name),
    CHECK (
        (
            previous_signing_secret_ref IS NULL
            AND previous_secret_valid_until IS NULL
        )
        OR (
            previous_signing_secret_ref IS NOT NULL
            AND btrim(previous_signing_secret_ref) <> ''
            AND previous_secret_valid_until IS NOT NULL
        )
    )
);

CREATE TRIGGER webhook_endpoints_set_updated_at
BEFORE UPDATE ON webhook_endpoints
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE webhook_deliveries (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    outbox_event_id uuid NOT NULL,
    endpoint_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'processing', 'delivered', 'dead')),
    attempt_count integer NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    max_attempts integer NOT NULL CHECK (max_attempts > 0),
    available_at timestamptz NOT NULL DEFAULT now(),
    locked_at timestamptz,
    lock_owner text,
    lease_expires_at timestamptz,
    last_response_status smallint
        CHECK (last_response_status BETWEEN 100 AND 599),
    last_error_kind text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz,
    dead_at timestamptz,
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, outbox_event_id, endpoint_id),
    CONSTRAINT webhook_deliveries_outbox_event_fk
        FOREIGN KEY (workspace_id, outbox_event_id)
        REFERENCES outbox_events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT webhook_deliveries_endpoint_fk
        FOREIGN KEY (workspace_id, endpoint_id)
        REFERENCES webhook_endpoints (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (attempt_count <= max_attempts),
    CHECK (
        (
            status = 'processing'
            AND locked_at IS NOT NULL
            AND lock_owner IS NOT NULL
            AND btrim(lock_owner) <> ''
            AND lease_expires_at IS NOT NULL
            AND lease_expires_at > locked_at
        )
        OR (
            status <> 'processing'
            AND locked_at IS NULL
            AND lock_owner IS NULL
            AND lease_expires_at IS NULL
        )
    ),
    CHECK ((status = 'delivered') = (delivered_at IS NOT NULL)),
    CHECK ((status = 'dead') = (dead_at IS NOT NULL))
);

CREATE TRIGGER webhook_deliveries_set_updated_at
BEFORE UPDATE ON webhook_deliveries
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX webhook_deliveries_claim_idx
    ON webhook_deliveries (status, available_at, id);
CREATE INDEX webhook_deliveries_lease_recovery_idx
    ON webhook_deliveries (lease_expires_at, id)
    WHERE status = 'processing';

CREATE TABLE webhook_delivery_attempts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    delivery_id uuid NOT NULL,
    attempt_number integer NOT NULL CHECK (attempt_number > 0),
    started_at timestamptz NOT NULL,
    finished_at timestamptz NOT NULL,
    outcome text NOT NULL CHECK (outcome IN ('delivered', 'retry', 'dead')),
    response_status smallint CHECK (response_status BETWEEN 100 AND 599),
    error_kind text,
    duration_ms integer NOT NULL CHECK (duration_ms >= 0),
    UNIQUE (workspace_id, delivery_id, attempt_number),
    CONSTRAINT webhook_delivery_attempts_delivery_fk
        FOREIGN KEY (workspace_id, delivery_id)
        REFERENCES webhook_deliveries (workspace_id, id)
        ON DELETE CASCADE,
    CHECK (finished_at >= started_at),
    CHECK (
        outcome <> 'delivered'
        OR (
            response_status IS NOT NULL
            AND response_status BETWEEN 200 AND 299
        )
    )
);

CREATE INDEX webhook_delivery_attempts_retention_idx
    ON webhook_delivery_attempts (finished_at);

CREATE TABLE webhook_replay_keys (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    source text NOT NULL CHECK (btrim(source) <> ''),
    event_id text NOT NULL UNIQUE CHECK (btrim(event_id) <> ''),
    body_sha256 bytea NOT NULL CHECK (octet_length(body_sha256) = 32),
    signed_at timestamptz NOT NULL,
    received_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, source, event_id),
    CHECK (expires_at > received_at)
);

CREATE INDEX webhook_replay_keys_expiry_idx
    ON webhook_replay_keys (expires_at);

CREATE TABLE idempotency_keys (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    scope text NOT NULL
        CHECK (btrim(scope) <> '' AND char_length(scope) <= 255),
    key text NOT NULL
        CHECK (btrim(key) <> '' AND char_length(key) <= 255),
    request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
    state text NOT NULL DEFAULT 'in_progress'
        CHECK (state IN ('in_progress', 'completed')),
    lease_owner text,
    lease_expires_at timestamptz,
    response_status integer CHECK (response_status BETWEEN 100 AND 599),
    response_body jsonb,
    response_content_type text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    completed_at timestamptz,
    expires_at timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, scope, key),
    CHECK (expires_at > created_at),
    CHECK (
        (
            state = 'in_progress'
            AND lease_owner IS NOT NULL
            AND btrim(lease_owner) <> ''
            AND lease_expires_at IS NOT NULL
            AND lease_expires_at > created_at
            AND lease_expires_at <= expires_at
            AND response_status IS NULL
            AND response_body IS NULL
            AND response_content_type IS NULL
            AND completed_at IS NULL
        )
        OR (
            state = 'completed'
            AND lease_owner IS NULL
            AND lease_expires_at IS NULL
            AND response_status IS NOT NULL
            AND completed_at IS NOT NULL
            AND completed_at >= created_at
            AND completed_at < expires_at
        )
    )
);

CREATE TRIGGER idempotency_keys_set_updated_at
BEFORE UPDATE ON idempotency_keys
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX idempotency_keys_expiry_idx
    ON idempotency_keys (expires_at);
CREATE INDEX idempotency_keys_lease_recovery_idx
    ON idempotency_keys (lease_expires_at)
    WHERE state = 'in_progress';
