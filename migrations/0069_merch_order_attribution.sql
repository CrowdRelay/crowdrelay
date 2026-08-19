-- Durable, exact attribution for paid merch orders.
--
-- A fact is accepted only after the canonical inventory reservation has been
-- committed. Event pickup rows must point at a canonical CrowdRelay event,
-- which makes show-level revenue and pack lists evidence-based rather than
-- inferred from timestamps.

CREATE TABLE merch_order_facts (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    stripe_session_id text NOT NULL CHECK (
        btrim(stripe_session_id) <> '' AND char_length(stripe_session_id) <= 255
    ),
    inventory_reservation_id uuid NOT NULL,
    fan_id uuid,
    event_id uuid,
    fulfillment_mode text NOT NULL CHECK (
        fulfillment_mode IN ('inpost', 'event_pickup', 'none')
    ),
    currency char(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    amount_gross_minor bigint NOT NULL CHECK (amount_gross_minor >= 0),
    goods_gross_minor bigint NOT NULL CHECK (goods_gross_minor >= 0),
    shipping_gross_minor bigint NOT NULL CHECK (shipping_gross_minor >= 0),
    confirmed_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, stripe_session_id),
    UNIQUE (workspace_id, inventory_reservation_id),
    CONSTRAINT merch_order_facts_reservation_fk
        FOREIGN KEY (workspace_id, inventory_reservation_id)
        REFERENCES inventory_reservations (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT merch_order_facts_fan_fk
        FOREIGN KEY (workspace_id, fan_id)
        REFERENCES fans (workspace_id, id)
        ON DELETE SET NULL,
    CONSTRAINT merch_order_facts_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (
        (fulfillment_mode = 'event_pickup' AND event_id IS NOT NULL)
        OR (fulfillment_mode <> 'event_pickup' AND event_id IS NULL)
    )
);

CREATE INDEX merch_order_facts_event_idx
    ON merch_order_facts (workspace_id, event_id, confirmed_at DESC, id)
    WHERE event_id IS NOT NULL;

CREATE INDEX merch_order_facts_fan_idx
    ON merch_order_facts (workspace_id, fan_id, confirmed_at DESC, id)
    WHERE fan_id IS NOT NULL;
