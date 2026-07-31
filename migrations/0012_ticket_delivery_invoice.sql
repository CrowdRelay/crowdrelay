-- Ticket wallet delivery, invoice details, and idempotent resend requests.

ALTER TABLE ticket_orders
    ADD COLUMN buyer_locale text NOT NULL DEFAULT 'pl' CHECK (buyer_locale IN ('pl', 'en')),
    ADD COLUMN invoice_buyer_type text CHECK (invoice_buyer_type IS NULL OR invoice_buyer_type IN ('individual', 'company')),
    ADD COLUMN invoice_company_name text CHECK (invoice_company_name IS NULL OR (btrim(invoice_company_name) <> '' AND char_length(invoice_company_name) <= 200)),
    ADD COLUMN invoice_tax_id text CHECK (invoice_tax_id IS NULL OR (btrim(invoice_tax_id) <> '' AND char_length(invoice_tax_id) <= 32)),
    ADD COLUMN invoice_full_name text CHECK (invoice_full_name IS NULL OR (btrim(invoice_full_name) <> '' AND char_length(invoice_full_name) <= 200)),
    ADD COLUMN invoice_address_line1 text CHECK (invoice_address_line1 IS NULL OR (btrim(invoice_address_line1) <> '' AND char_length(invoice_address_line1) <= 240)),
    ADD COLUMN invoice_postal_code text CHECK (invoice_postal_code IS NULL OR (btrim(invoice_postal_code) <> '' AND char_length(invoice_postal_code) <= 32)),
    ADD COLUMN invoice_city text CHECK (invoice_city IS NULL OR (btrim(invoice_city) <> '' AND char_length(invoice_city) <= 120)),
    ADD COLUMN invoice_country_code char(2) CHECK (invoice_country_code IS NULL OR invoice_country_code ~ '^[A-Z]{2}$'),
    ADD COLUMN last_delivery_requested_at timestamptz,
    ADD COLUMN delivery_request_count integer NOT NULL DEFAULT 0 CHECK (delivery_request_count >= 0),
    ADD CONSTRAINT ticket_orders_invoice_details_check CHECK (
        (
            invoice_requested = false
            AND invoice_buyer_type IS NULL
            AND invoice_company_name IS NULL
            AND invoice_tax_id IS NULL
            AND invoice_full_name IS NULL
            AND invoice_address_line1 IS NULL
            AND invoice_postal_code IS NULL
            AND invoice_city IS NULL
            AND invoice_country_code IS NULL
        )
        OR (
            invoice_requested = true
            AND invoice_buyer_type = 'individual'
            AND invoice_full_name IS NOT NULL
            AND invoice_company_name IS NULL
            AND invoice_tax_id IS NULL
            AND invoice_address_line1 IS NOT NULL
            AND invoice_postal_code IS NOT NULL
            AND invoice_city IS NOT NULL
            AND invoice_country_code IS NOT NULL
        )
        OR (
            invoice_requested = true
            AND invoice_buyer_type = 'company'
            AND invoice_company_name IS NOT NULL
            AND invoice_tax_id IS NOT NULL
            AND invoice_full_name IS NULL
            AND invoice_address_line1 IS NOT NULL
            AND invoice_postal_code IS NOT NULL
            AND invoice_city IS NOT NULL
            AND invoice_country_code IS NOT NULL
        )
    ) NOT VALID;

-- Legacy Stage 2 rows may have requested an invoice before buyer details were
-- collected. PostgreSQL enforces this constraint for every new or updated row;
-- validate it later after any such historical rows are completed manually.

CREATE TABLE ticket_delivery_requests (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    ticket_order_id uuid NOT NULL,
    idempotency_key text NOT NULL CHECK (btrim(idempotency_key) <> '' AND char_length(idempotency_key) <= 128),
    request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, idempotency_key),
    CONSTRAINT ticket_delivery_requests_order_fk
        FOREIGN KEY (workspace_id, ticket_order_id)
        REFERENCES ticket_orders (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX ticket_delivery_requests_order_idx
    ON ticket_delivery_requests (workspace_id, ticket_order_id, created_at DESC, id DESC);
