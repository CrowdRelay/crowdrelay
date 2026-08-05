-- Additive commerce inventory and reward-campaign integration.
--
-- This migration deliberately does not alter ticketing, fan lifecycle, outbox
-- delivery or existing reward draw rows. New public behavior remains disabled
-- through feature flags until catalog and stock have been verified.

INSERT INTO ecosystem_feature_flags (workspace_id, key, enabled, reason)
SELECT workspace.id, flag.key, false, 'staged rollout'
FROM workspaces AS workspace
CROSS JOIN (
    VALUES
        ('merch_inventory_enabled'),
        ('reward_campaigns_enabled'),
        ('merch_inventory_writes_enabled')
) AS flag(key)
ON CONFLICT (workspace_id, key) DO NOTHING;

CREATE TABLE merch_products (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    name text NOT NULL CHECK (btrim(name) <> '' AND char_length(name) <= 200),
    description text CHECK (description IS NULL OR char_length(description) <= 2000),
    image_url text CHECK (image_url IS NULL OR image_url ~* '^https://'),
    currency char(3) NOT NULL DEFAULT 'PLN' CHECK (currency ~ '^[A-Z]{3}$'),
    price_gross_minor bigint NOT NULL CHECK (price_gross_minor >= 0),
    active boolean NOT NULL DEFAULT true,
    public boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug)
);

CREATE TRIGGER merch_products_set_updated_at
BEFORE UPDATE ON merch_products
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE merch_variants (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    product_id uuid NOT NULL,
    sku text NOT NULL CHECK (btrim(sku) <> '' AND char_length(sku) <= 128),
    label text NOT NULL CHECK (btrim(label) <> '' AND char_length(label) <= 160),
    attributes jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(attributes) = 'object'),
    active boolean NOT NULL DEFAULT true,
    low_stock_threshold integer NOT NULL DEFAULT 3 CHECK (low_stock_threshold >= 0),
    sell_without_stock boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, sku),
    CONSTRAINT merch_variants_product_fk
        FOREIGN KEY (workspace_id, product_id)
        REFERENCES merch_products (workspace_id, id)
        ON DELETE CASCADE
);

CREATE TRIGGER merch_variants_set_updated_at
BEFORE UPDATE ON merch_variants
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX merch_variants_product_idx
    ON merch_variants (workspace_id, product_id, active, label, id);

CREATE TABLE inventory_reservations (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    reservation_kind text NOT NULL CHECK (reservation_kind IN ('order', 'campaign', 'operational')),
    external_reference text NOT NULL CHECK (
        btrim(external_reference) <> '' AND char_length(external_reference) <= 200
    ),
    request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
    status text NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'committed', 'released', 'expired')),
    expires_at timestamptz,
    committed_at timestamptz,
    released_at timestamptz,
    release_reason text CHECK (release_reason IS NULL OR char_length(release_reason) <= 240),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, reservation_kind, external_reference),
    CHECK (reservation_kind = 'campaign' OR expires_at IS NOT NULL),
    CHECK (
        (status = 'active' AND committed_at IS NULL AND released_at IS NULL)
        OR (status = 'committed' AND committed_at IS NOT NULL AND released_at IS NULL)
        OR (status IN ('released', 'expired') AND committed_at IS NULL AND released_at IS NOT NULL)
    )
);

CREATE TRIGGER inventory_reservations_set_updated_at
BEFORE UPDATE ON inventory_reservations
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX inventory_reservations_expiry_idx
    ON inventory_reservations (expires_at, id)
    WHERE status = 'active' AND expires_at IS NOT NULL;

CREATE TABLE inventory_reservation_items (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    reservation_id uuid NOT NULL,
    variant_id uuid NOT NULL,
    quantity integer NOT NULL CHECK (quantity > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, reservation_id, variant_id),
    CONSTRAINT inventory_reservation_items_reservation_fk
        FOREIGN KEY (workspace_id, reservation_id)
        REFERENCES inventory_reservations (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT inventory_reservation_items_variant_fk
        FOREIGN KEY (workspace_id, variant_id)
        REFERENCES merch_variants (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX inventory_reservation_items_variant_idx
    ON inventory_reservation_items (workspace_id, variant_id, reservation_id);

CREATE TABLE inventory_ledger (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    variant_id uuid NOT NULL,
    delta integer NOT NULL CHECK (delta <> 0),
    movement_kind text NOT NULL CHECK (movement_kind IN (
        'initial', 'receipt', 'sale', 'refund', 'adjustment',
        'promotional_issue', 'damage', 'staff_issue'
    )),
    idempotency_key text NOT NULL CHECK (
        btrim(idempotency_key) <> '' AND char_length(idempotency_key) <= 200
    ),
    reservation_id uuid,
    actor_kind text NOT NULL DEFAULT 'system'
        CHECK (actor_kind IN ('system', 'admin', 'staff', 'stripe')),
    actor_id text CHECK (actor_id IS NULL OR char_length(actor_id) <= 200),
    reason text CHECK (reason IS NULL OR char_length(reason) <= 500),
    occurred_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, idempotency_key),
    CONSTRAINT inventory_ledger_variant_fk
        FOREIGN KEY (workspace_id, variant_id)
        REFERENCES merch_variants (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT inventory_ledger_reservation_fk
        FOREIGN KEY (workspace_id, reservation_id)
        REFERENCES inventory_reservations (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE INDEX inventory_ledger_variant_time_idx
    ON inventory_ledger (workspace_id, variant_id, occurred_at DESC, id DESC);

CREATE TABLE reward_draw_inventory_allocations (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    draw_id uuid NOT NULL,
    variant_id uuid NOT NULL,
    reservation_id uuid NOT NULL,
    units_per_winner integer NOT NULL DEFAULT 1 CHECK (units_per_winner > 0),
    reserved_quantity integer NOT NULL CHECK (reserved_quantity > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, draw_id),
    CONSTRAINT reward_draw_inventory_allocations_draw_fk
        FOREIGN KEY (workspace_id, draw_id)
        REFERENCES reward_draws (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draw_inventory_allocations_variant_fk
        FOREIGN KEY (workspace_id, variant_id)
        REFERENCES merch_variants (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT reward_draw_inventory_allocations_reservation_fk
        FOREIGN KEY (workspace_id, reservation_id)
        REFERENCES inventory_reservations (workspace_id, id)
        ON DELETE RESTRICT
);

CREATE TABLE reward_draw_fulfillments (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    draw_id uuid NOT NULL,
    winner_id uuid NOT NULL,
    reward_grant_id uuid NOT NULL,
    variant_id uuid NOT NULL,
    quantity integer NOT NULL CHECK (quantity > 0),
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'prepared', 'delivered', 'cancelled')),
    prepared_at timestamptz,
    delivered_at timestamptz,
    cancelled_at timestamptz,
    actor_id text CHECK (actor_id IS NULL OR char_length(actor_id) <= 200),
    note text CHECK (note IS NULL OR char_length(note) <= 500),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, winner_id),
    CONSTRAINT reward_draw_fulfillments_draw_fk
        FOREIGN KEY (workspace_id, draw_id)
        REFERENCES reward_draws (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draw_fulfillments_winner_fk
        FOREIGN KEY (workspace_id, winner_id)
        REFERENCES reward_draw_winners (workspace_id, id)
        ON DELETE CASCADE,
    CONSTRAINT reward_draw_fulfillments_grant_fk
        FOREIGN KEY (workspace_id, reward_grant_id)
        REFERENCES reward_grants (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT reward_draw_fulfillments_variant_fk
        FOREIGN KEY (workspace_id, variant_id)
        REFERENCES merch_variants (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (
        (status = 'pending' AND prepared_at IS NULL AND delivered_at IS NULL AND cancelled_at IS NULL)
        OR (status = 'prepared' AND prepared_at IS NOT NULL AND delivered_at IS NULL AND cancelled_at IS NULL)
        OR (status = 'delivered' AND delivered_at IS NOT NULL AND cancelled_at IS NULL)
        OR (status = 'cancelled' AND delivered_at IS NULL AND cancelled_at IS NOT NULL)
    )
);

CREATE TRIGGER reward_draw_fulfillments_set_updated_at
BEFORE UPDATE ON reward_draw_fulfillments
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE INDEX reward_draw_fulfillments_status_idx
    ON reward_draw_fulfillments (workspace_id, status, created_at, id);
