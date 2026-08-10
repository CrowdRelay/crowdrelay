-- VIRYA AREA wallet authority moves fully into CrowdRelay/PostgreSQL.
-- Netlify Blobs remain a one-way legacy import source only.

CREATE TABLE IF NOT EXISTS area_credit_ledger (
    workspace_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT gen_random_uuid(),
    player_id uuid NOT NULL,
    delta integer NOT NULL,
    reason text NOT NULL,
    reference_key text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, player_id, reference_key),
    FOREIGN KEY (workspace_id, player_id)
        REFERENCES area_players(workspace_id, id) ON DELETE CASCADE,
    CHECK (delta <> 0),
    CHECK (reason IN (
        'claim', 'legacy_balance_import',
        'voucher_spend', 'voucher_refund',
        'ticket_reward_spend', 'ticket_reward_refund',
        'operator_adjustment'
    )),
    CHECK (length(reference_key) BETWEEN 1 AND 256)
);

CREATE INDEX IF NOT EXISTS area_credit_ledger_player_idx
    ON area_credit_ledger (workspace_id, player_id, created_at DESC);

-- Existing canonical claims are worth one Credit each. ON CONFLICT keeps this
-- safe on retries and when the migration is applied to a database that has
-- already been partially backfilled.
INSERT INTO area_credit_ledger (
    workspace_id, player_id, delta, reason, reference_key, created_at
)
SELECT
    workspace_id,
    player_id,
    1,
    'claim',
    'claim:' || drop_id,
    claimed_at
FROM area_claims
ON CONFLICT (workspace_id, player_id, reference_key) DO NOTHING;

CREATE TABLE IF NOT EXISTS area_reward_vouchers (
    workspace_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT gen_random_uuid(),
    player_id uuid NOT NULL,
    request_id uuid NOT NULL,
    code text NOT NULL,
    code_hash bytea NOT NULL,
    code_suffix text NOT NULL,
    token_cost integer NOT NULL DEFAULT 1,
    benefit text NOT NULL DEFAULT 'free-item-and-shipping',
    status text NOT NULL DEFAULT 'issued',
    issued_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    reservation_id text,
    reserved_until timestamptz,
    checkout_session_id text,
    free_product_id text,
    free_product_label text,
    redeemed_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, request_id),
    UNIQUE (workspace_id, code),
    UNIQUE (workspace_id, code_hash),
    FOREIGN KEY (workspace_id, player_id)
        REFERENCES area_players(workspace_id, id) ON DELETE CASCADE,
    CHECK (code ~ '^VIRYA-[A-Z0-9]{4}(-[A-Z0-9]{4}){5}$'),
    CHECK (code_suffix ~ '^[A-Z0-9]{4}$'),
    CHECK (octet_length(code_hash) = 32),
    CHECK (token_cost BETWEEN 1 AND 20),
    CHECK (benefit = 'free-item-and-shipping'),
    CHECK (status IN ('issued', 'reserved', 'redeemed', 'failed')),
    CHECK (expires_at > issued_at),
    CHECK ((status <> 'reserved') OR (reservation_id IS NOT NULL AND reserved_until IS NOT NULL)),
    CHECK ((status <> 'redeemed') OR redeemed_at IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS area_reward_vouchers_player_idx
    ON area_reward_vouchers (workspace_id, player_id, issued_at DESC);
CREATE INDEX IF NOT EXISTS area_reward_vouchers_lease_idx
    ON area_reward_vouchers (workspace_id, reserved_until)
    WHERE status = 'reserved';

CREATE TABLE IF NOT EXISTS area_ticket_rewards (
    workspace_id uuid NOT NULL,
    id uuid NOT NULL DEFAULT gen_random_uuid(),
    player_id uuid NOT NULL,
    request_id uuid NOT NULL,
    event_slug text NOT NULL,
    credits integer NOT NULL,
    fan_email text NOT NULL,
    status text NOT NULL DEFAULT 'reserved',
    reservation_id text NOT NULL,
    reservation_expires_at timestamptz NOT NULL,
    public_reference text,
    issued_at timestamptz,
    failure_code text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, id),
    UNIQUE (workspace_id, request_id),
    FOREIGN KEY (workspace_id, player_id)
        REFERENCES area_players(workspace_id, id) ON DELETE CASCADE,
    CHECK (event_slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    CHECK (credits BETWEEN 1 AND 20),
    CHECK (fan_email = lower(btrim(fan_email))),
    CHECK (length(fan_email) BETWEEN 3 AND 320),
    CHECK (status IN ('reserved', 'issued', 'failed')),
    CHECK (reservation_expires_at > created_at),
    CHECK ((status <> 'issued') OR (public_reference IS NOT NULL AND issued_at IS NOT NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS area_ticket_rewards_one_issued_event_idx
    ON area_ticket_rewards (workspace_id, player_id, event_slug)
    WHERE status = 'issued';
CREATE INDEX IF NOT EXISTS area_ticket_rewards_player_idx
    ON area_ticket_rewards (workspace_id, player_id, created_at DESC);
CREATE INDEX IF NOT EXISTS area_ticket_rewards_lease_idx
    ON area_ticket_rewards (workspace_id, reservation_expires_at)
    WHERE status = 'reserved';

CREATE TABLE IF NOT EXISTS area_legacy_wallet_imports (
    workspace_id uuid NOT NULL,
    player_id uuid NOT NULL,
    migration_id text NOT NULL,
    source_balance integer NOT NULL,
    source_voucher_count integer NOT NULL DEFAULT 0,
    source_ticket_reward_count integer NOT NULL DEFAULT 0,
    imported_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, player_id),
    UNIQUE (workspace_id, migration_id),
    FOREIGN KEY (workspace_id, player_id)
        REFERENCES area_players(workspace_id, id) ON DELETE CASCADE,
    CHECK (length(migration_id) BETWEEN 8 AND 128),
    CHECK (source_balance >= 0),
    CHECK (source_voucher_count BETWEEN 0 AND 1000),
    CHECK (source_ticket_reward_count BETWEEN 0 AND 1000)
);

CREATE UNIQUE INDEX IF NOT EXISTS area_ticket_rewards_active_event_idx
    ON area_ticket_rewards (workspace_id, player_id, event_slug)
    WHERE status IN ('reserved', 'issued');
