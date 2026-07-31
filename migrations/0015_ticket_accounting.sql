-- Stable Polish ticket-sales accounting snapshots. Monetary values are stored
-- in minor currency units. The ledger is append-only and event-timed so a
-- later refund cannot rewrite a previously closed month.

CREATE TABLE ticket_accounting_profiles (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    seller_name text NOT NULL CHECK (btrim(seller_name) <> '' AND char_length(seller_name) <= 200),
    tax_id text NOT NULL CHECK (btrim(tax_id) <> '' AND char_length(tax_id) <= 32),
    regon text CHECK (regon IS NULL OR (btrim(regon) <> '' AND char_length(regon) <= 32)),
    address_line1 text NOT NULL CHECK (btrim(address_line1) <> '' AND char_length(address_line1) <= 240),
    postal_code text NOT NULL CHECK (btrim(postal_code) <> '' AND char_length(postal_code) <= 32),
    city text NOT NULL CHECK (btrim(city) <> '' AND char_length(city) <= 120),
    country_code char(2) NOT NULL DEFAULT 'PL' CHECK (country_code ~ '^[A-Z]{2}$'),
    document_prefix text NOT NULL DEFAULT 'WEW/BILETY' CHECK (
        btrim(document_prefix) <> '' AND char_length(document_prefix) <= 64
    ),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TRIGGER ticket_accounting_profiles_set_updated_at
BEFORE UPDATE ON ticket_accounting_profiles
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE ticket_accounting_entries (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    ticket_order_id uuid NOT NULL,
    event_id uuid NOT NULL,
    stripe_event_id text NOT NULL CHECK (btrim(stripe_event_id) <> '' AND char_length(stripe_event_id) <= 255),
    entry_kind text NOT NULL CHECK (entry_kind IN ('sale', 'refund')),
    occurred_at timestamptz NOT NULL,
    currency char(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    vat_rate_basis_points integer NOT NULL CHECK (vat_rate_basis_points BETWEEN 0 AND 10000),
    amount_gross_minor bigint NOT NULL,
    amount_net_minor bigint NOT NULL,
    amount_vat_minor bigint NOT NULL,
    stripe_balance_transaction_id text CHECK (
        stripe_balance_transaction_id IS NULL OR (
            btrim(stripe_balance_transaction_id) <> ''
            AND char_length(stripe_balance_transaction_id) <= 255
        )
    ),
    stripe_fee_minor bigint,
    stripe_net_minor bigint,
    stripe_reporting_category text CHECK (
        stripe_reporting_category IS NULL OR char_length(stripe_reporting_category) <= 80
    ),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, stripe_event_id),
    CONSTRAINT ticket_accounting_entries_order_fk
        FOREIGN KEY (workspace_id, ticket_order_id)
        REFERENCES ticket_orders (workspace_id, id)
        ON DELETE RESTRICT,
    CONSTRAINT ticket_accounting_entries_event_fk
        FOREIGN KEY (workspace_id, event_id)
        REFERENCES events (workspace_id, id)
        ON DELETE RESTRICT,
    CHECK (amount_net_minor + amount_vat_minor = amount_gross_minor),
    CHECK (
        (entry_kind = 'sale' AND amount_gross_minor >= 0 AND amount_net_minor >= 0 AND amount_vat_minor >= 0)
        OR
        (entry_kind = 'refund' AND amount_gross_minor <= 0 AND amount_net_minor <= 0 AND amount_vat_minor <= 0)
    ),
    CHECK (
        (stripe_fee_minor IS NULL AND stripe_net_minor IS NULL)
        OR
        (stripe_fee_minor IS NOT NULL AND stripe_net_minor IS NOT NULL
         AND amount_gross_minor - stripe_fee_minor = stripe_net_minor)
    )
);

CREATE INDEX ticket_accounting_entries_period_idx
    ON ticket_accounting_entries (workspace_id, occurred_at, currency, entry_kind, id);
CREATE INDEX ticket_accounting_entries_order_idx
    ON ticket_accounting_entries (workspace_id, ticket_order_id, occurred_at, id);
CREATE UNIQUE INDEX ticket_accounting_balance_tx_unique
    ON ticket_accounting_entries (workspace_id, stripe_balance_transaction_id)
    WHERE stripe_balance_transaction_id IS NOT NULL;

-- Backfill historical paid orders so upgrading an already-live installation
-- does not create an empty first accounting month. Synthetic identifiers are
-- namespaced and cannot collide with Stripe's evt_ identifiers.
INSERT INTO ticket_accounting_entries (
    workspace_id, ticket_order_id, event_id, stripe_event_id, entry_kind,
    occurred_at, currency, vat_rate_basis_points, amount_gross_minor,
    amount_net_minor, amount_vat_minor
)
SELECT
    orders.workspace_id,
    orders.id,
    sale.event_id,
    'backfill-sale:' || orders.id::text,
    'sale',
    orders.paid_at,
    orders.currency,
    orders.vat_rate_basis_points,
    orders.amount_gross_minor,
    orders.amount_net_minor,
    orders.amount_vat_minor
FROM ticket_orders AS orders
JOIN ticket_sales AS sale
  ON sale.workspace_id = orders.workspace_id
 AND sale.id = orders.ticket_sale_id
WHERE orders.paid_at IS NOT NULL
  AND orders.status IN ('paid', 'partially_refunded', 'refunded')
ON CONFLICT DO NOTHING;

INSERT INTO ticket_accounting_entries (
    workspace_id, ticket_order_id, event_id, stripe_event_id, entry_kind,
    occurred_at, currency, vat_rate_basis_points, amount_gross_minor,
    amount_net_minor, amount_vat_minor
)
SELECT
    orders.workspace_id,
    orders.id,
    sale.event_id,
    'backfill-refund:' || orders.id::text,
    'refund',
    COALESCE(orders.refunded_at, orders.updated_at),
    orders.currency,
    orders.vat_rate_basis_points,
    -orders.amount_refunded_minor,
    CASE
        WHEN orders.amount_gross_minor = 0 THEN 0
        WHEN orders.amount_refunded_minor = orders.amount_gross_minor THEN -orders.amount_net_minor
        ELSE -((orders.amount_refunded_minor * orders.amount_net_minor) / orders.amount_gross_minor)
    END,
    CASE
        WHEN orders.amount_gross_minor = 0 THEN 0
        WHEN orders.amount_refunded_minor = orders.amount_gross_minor THEN -orders.amount_vat_minor
        ELSE -orders.amount_refunded_minor
             + ((orders.amount_refunded_minor * orders.amount_net_minor) / orders.amount_gross_minor)
    END
FROM ticket_orders AS orders
JOIN ticket_sales AS sale
  ON sale.workspace_id = orders.workspace_id
 AND sale.id = orders.ticket_sale_id
WHERE orders.amount_refunded_minor > 0
  AND orders.status IN ('partially_refunded', 'refunded')
ON CONFLICT DO NOTHING;

CREATE TABLE ticket_accounting_documents (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    period_start date NOT NULL,
    period_end date NOT NULL,
    document_number text NOT NULL CHECK (
        btrim(document_number) <> '' AND char_length(document_number) <= 100
    ),
    document_kind text NOT NULL DEFAULT 'WEW' CHECK (document_kind = 'WEW'),
    currency char(3) NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
    gross_minor bigint NOT NULL,
    net_minor bigint NOT NULL,
    vat_minor bigint NOT NULL,
    stripe_fee_minor bigint NOT NULL DEFAULT 0,
    stripe_net_minor bigint NOT NULL DEFAULT 0,
    snapshot jsonb NOT NULL,
    finalized_at timestamptz NOT NULL DEFAULT now(),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, period_start, period_end, currency),
    UNIQUE (workspace_id, document_number),
    CHECK (period_start <= period_end),
    CHECK (net_minor + vat_minor = gross_minor)
);

CREATE INDEX ticket_accounting_documents_period_idx
    ON ticket_accounting_documents (workspace_id, period_start DESC, currency, id DESC);


-- Accounting entries and finalized monthly snapshots are immutable evidence.
-- Corrections are represented by new refund entries or a new accounting
-- period, never by rewriting historical rows.
CREATE OR REPLACE FUNCTION crowdrelay_reject_accounting_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'accounting evidence is append-only';
END;
$$;

CREATE TRIGGER ticket_accounting_entries_append_only
BEFORE UPDATE OR DELETE ON ticket_accounting_entries
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_accounting_mutation();

CREATE TRIGGER ticket_accounting_documents_append_only
BEFORE UPDATE OR DELETE ON ticket_accounting_documents
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_reject_accounting_mutation();
