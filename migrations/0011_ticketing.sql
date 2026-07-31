-- First-party paid ticketing built on the existing admission credential.
--
-- A ticket sale owns one admission pool (shared venue capacity) and one or more
-- priced ticket types. Pending orders are durable inventory holds. A verified
-- Stripe webhook converts the hold atomically into claimed admission passes,
-- so paid tickets, draw prizes and manually issued guest-list passes share the
-- same one-time gate redemption path.

-- Pending Stripe checkouts reserve the same physical capacity used by manual
-- passes and draw prizes. The invariant is enforced in PostgreSQL, not only in
-- application code, so concurrent issuance cannot oversell the venue.
ALTER TABLE admission_pools
    ADD COLUMN reserved_count integer NOT NULL DEFAULT 0
        CHECK (reserved_count >= 0),
    ADD CONSTRAINT admission_pools_total_commitment_check
        CHECK (issued_count + reserved_count <= capacity);

CREATE TABLE ticket_sales (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    event_id uuid NOT NULL,
    admission_pool_id uuid NOT NULL,
    currency char(3) NOT NULL DEFAULT 'PLN' CHECK (currency ~ '^[A-Z]{3}$'),
    vat_rate_basis_points integer NOT NULL DEFAULT 800
        CHECK (vat_rate_basis_points BETWEEN 0 AND 10000),
    capacity integer NOT NULL CHECK (capacity BETWEEN 1 AND 1000000),
    max_per_order integer NOT NULL DEFAULT 8 CHECK (max_per_order BETWEEN 1 AND 100),
    hold_seconds integer NOT NULL DEFAULT 2100 CHECK (hold_seconds BETWEEN 2100 AND 86400),
    sales_open_at timestamptz NOT NULL,
    sales_close_at timestamptz NOT NULL,
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, event_id),
    UNIQUE (workspace_id, admission_pool_id),
    CONSTRAINT ticket_sales_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT ticket_sales_pool_event_fk
        FOREIGN KEY (workspace_id, admission_pool_id, event_id)
        REFERENCES admission_pools (workspace_id, id, event_id)
        ON DELETE RESTRICT,
    CHECK (sales_open_at < sales_close_at)
);

CREATE TRIGGER ticket_sales_set_updated_at
BEFORE UPDATE ON ticket_sales
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX ticket_sales_public_idx
    ON ticket_sales (workspace_id, active, sales_open_at, sales_close_at, event_id);

CREATE TABLE ticket_types (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    ticket_sale_id uuid NOT NULL,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    name text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 160),
    description text CHECK (description IS NULL OR char_length(description) <= 1000),
    price_gross_minor bigint NOT NULL CHECK (price_gross_minor BETWEEN 1 AND 1000000000),
    capacity integer CHECK (capacity IS NULL OR capacity BETWEEN 1 AND 1000000),
    sort_order integer NOT NULL DEFAULT 0 CHECK (sort_order BETWEEN -100000 AND 100000),
    active boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, ticket_sale_id, slug),
    CONSTRAINT ticket_types_sale_fk
        FOREIGN KEY (workspace_id, ticket_sale_id)
        REFERENCES ticket_sales (workspace_id, id)
        ON DELETE CASCADE
);

CREATE TRIGGER ticket_types_set_updated_at
BEFORE UPDATE ON ticket_types
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX ticket_types_public_idx
    ON ticket_types (workspace_id, ticket_sale_id, active, sort_order, id);

CREATE TABLE ticket_orders (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    ticket_sale_id uuid NOT NULL,
    public_reference text NOT NULL CHECK (
        public_reference ~ '^VRY-ORD-[A-F0-9]{16}$'
    ),
    status text NOT NULL DEFAULT 'reserved' CHECK (
        status IN (
            'reserved', 'checkout_created', 'paid', 'partially_refunded',
            'refunded', 'expired', 'cancelled', 'payment_failed'
        )
    ),
    buyer_email text NOT NULL CHECK (btrim(buyer_email) <> '' AND char_length(buyer_email) <= 320),
    buyer_name text CHECK (buyer_name IS NULL OR (btrim(buyer_name) <> '' AND char_length(buyer_name) <= 200)),
    currency char(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    amount_gross_minor bigint NOT NULL CHECK (amount_gross_minor >= 0),
    amount_net_minor bigint NOT NULL CHECK (amount_net_minor >= 0),
    amount_vat_minor bigint NOT NULL CHECK (amount_vat_minor >= 0),
    amount_refunded_minor bigint NOT NULL DEFAULT 0 CHECK (amount_refunded_minor >= 0),
    vat_rate_basis_points integer NOT NULL CHECK (vat_rate_basis_points BETWEEN 0 AND 10000),
    invoice_requested boolean NOT NULL DEFAULT false,
    reservation_key text NOT NULL CHECK (btrim(reservation_key) <> '' AND char_length(reservation_key) <= 128),
    request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
    checkout_token_hash bytea NOT NULL CHECK (octet_length(checkout_token_hash) = 32),
    expires_at timestamptz NOT NULL,
    stripe_checkout_session_id text UNIQUE
        CHECK (stripe_checkout_session_id IS NULL OR (btrim(stripe_checkout_session_id) <> '' AND char_length(stripe_checkout_session_id) <= 255)),
    stripe_payment_intent_id text
        CHECK (stripe_payment_intent_id IS NULL OR (btrim(stripe_payment_intent_id) <> '' AND char_length(stripe_payment_intent_id) <= 255)),
    paid_at timestamptz,
    refunded_at timestamptz,
    released_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, public_reference),
    UNIQUE (workspace_id, reservation_key),
    CONSTRAINT ticket_orders_sale_fk
        FOREIGN KEY (workspace_id, ticket_sale_id)
        REFERENCES ticket_sales (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (amount_net_minor + amount_vat_minor = amount_gross_minor),
    CHECK (amount_refunded_minor <= amount_gross_minor),
    CHECK (expires_at > created_at),
    CHECK ((status IN ('paid', 'partially_refunded', 'refunded')) = (paid_at IS NOT NULL)),
    CHECK ((status = 'refunded') = (refunded_at IS NOT NULL)),
    CHECK (
        status NOT IN ('expired', 'cancelled', 'payment_failed')
        OR released_at IS NOT NULL
    )
);

CREATE TRIGGER ticket_orders_set_updated_at
BEFORE UPDATE ON ticket_orders
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX ticket_orders_sale_capacity_idx
    ON ticket_orders (workspace_id, ticket_sale_id, status, expires_at, id);
CREATE INDEX ticket_orders_buyer_time_idx
    ON ticket_orders (workspace_id, buyer_email, created_at DESC, id DESC);
CREATE UNIQUE INDEX ticket_orders_payment_intent_unique
    ON ticket_orders (workspace_id, stripe_payment_intent_id)
    WHERE stripe_payment_intent_id IS NOT NULL;

CREATE TABLE ticket_order_items (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    ticket_order_id uuid NOT NULL,
    ticket_type_id uuid NOT NULL,
    quantity integer NOT NULL CHECK (quantity BETWEEN 1 AND 100),
    unit_gross_minor bigint NOT NULL CHECK (unit_gross_minor >= 0),
    unit_net_minor bigint NOT NULL CHECK (unit_net_minor >= 0),
    unit_vat_minor bigint NOT NULL CHECK (unit_vat_minor >= 0),
    total_gross_minor bigint NOT NULL CHECK (total_gross_minor >= 0),
    total_net_minor bigint NOT NULL CHECK (total_net_minor >= 0),
    total_vat_minor bigint NOT NULL CHECK (total_vat_minor >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, ticket_order_id, ticket_type_id),
    CONSTRAINT ticket_order_items_order_fk
        FOREIGN KEY (workspace_id, ticket_order_id)
        REFERENCES ticket_orders (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT ticket_order_items_type_fk
        FOREIGN KEY (workspace_id, ticket_type_id)
        REFERENCES ticket_types (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (unit_net_minor + unit_vat_minor = unit_gross_minor),
    CHECK (total_net_minor + total_vat_minor = total_gross_minor),
    CHECK (total_gross_minor = unit_gross_minor * quantity)
);

CREATE INDEX ticket_order_items_type_capacity_idx
    ON ticket_order_items (workspace_id, ticket_type_id, ticket_order_id);

CREATE TABLE ticket_stripe_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    ticket_order_id uuid NOT NULL,
    stripe_event_id text NOT NULL CHECK (btrim(stripe_event_id) <> '' AND char_length(stripe_event_id) <= 255),
    event_type text NOT NULL CHECK (btrim(event_type) <> '' AND char_length(event_type) <= 128),
    payload_hash bytea NOT NULL CHECK (octet_length(payload_hash) = 32),
    processed_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, stripe_event_id),
    CONSTRAINT ticket_stripe_events_order_fk
        FOREIGN KEY (workspace_id, ticket_order_id)
        REFERENCES ticket_orders (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX ticket_stripe_events_order_idx
    ON ticket_stripe_events (workspace_id, ticket_order_id, processed_at DESC, id DESC);

ALTER TABLE admission_passes
    DROP CONSTRAINT IF EXISTS admission_passes_workspace_id_admission_pool_id_fan_id_key,
    DROP CONSTRAINT admission_passes_issuance_method_check,
    ADD COLUMN ticket_order_item_id uuid,
    ADD COLUMN ticket_sequence integer,
    ADD COLUMN holder_name text CHECK (
        holder_name IS NULL OR (btrim(holder_name) <> '' AND char_length(holder_name) <= 200)
    ),
    ADD COLUMN holder_email text CHECK (
        holder_email IS NULL OR (btrim(holder_email) <> '' AND char_length(holder_email) <= 320)
    ),
    ADD CONSTRAINT admission_passes_issuance_method_check CHECK (
        issuance_method IN ('manual', 'deterministic_reward', 'first_come', 'weighted_draw', 'paid')
    ),
    ADD CONSTRAINT admission_passes_ticket_item_fk
        FOREIGN KEY (workspace_id, ticket_order_item_id)
        REFERENCES ticket_order_items (workspace_id, id)
        ON DELETE RESTRICT,
    ADD CONSTRAINT admission_passes_paid_identity_check CHECK (
        (
            issuance_method = 'paid'
            AND ticket_order_item_id IS NOT NULL
            AND ticket_sequence BETWEEN 1 AND 100
            AND holder_email IS NOT NULL
        )
        OR (
            issuance_method <> 'paid'
            AND ticket_order_item_id IS NULL
            AND ticket_sequence IS NULL
        )
    );

CREATE UNIQUE INDEX admission_passes_non_paid_fan_pool_unique
    ON admission_passes (workspace_id, admission_pool_id, fan_id)
    WHERE issuance_method <> 'paid';

CREATE UNIQUE INDEX admission_passes_paid_item_sequence_unique
    ON admission_passes (workspace_id, ticket_order_item_id, ticket_sequence)
    WHERE issuance_method = 'paid';

CREATE INDEX admission_passes_ticket_order_item_idx
    ON admission_passes (workspace_id, ticket_order_item_id, ticket_sequence)
    WHERE ticket_order_item_id IS NOT NULL;
