-- Canonical Virya merch catalog, exact stocktakes and atomic inventory activation.
-- The tables and seed are additive. Existing ticketing, fan lifecycle, mail,
-- outbox and n8n contracts are deliberately untouched.

ALTER TABLE inventory_ledger
    DROP CONSTRAINT IF EXISTS inventory_ledger_movement_kind_check;
ALTER TABLE inventory_ledger
    ADD CONSTRAINT inventory_ledger_movement_kind_check CHECK (movement_kind IN (
        'initial', 'receipt', 'sale', 'refund', 'adjustment', 'stocktake',
        'promotional_issue', 'damage', 'staff_issue'
    ));

CREATE TABLE inventory_activation_state (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    status text NOT NULL DEFAULT 'preparing' CHECK (status IN ('preparing', 'ready')),
    catalog_seed_version integer NOT NULL DEFAULT 1 CHECK (catalog_seed_version > 0),
    catalog_seeded_at timestamptz,
    ready_at timestamptz,
    ready_by text CHECK (ready_by IS NULL OR char_length(ready_by) <= 200),
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((status = 'ready') = (ready_at IS NOT NULL))
);

CREATE TRIGGER inventory_activation_state_set_updated_at
BEFORE UPDATE ON inventory_activation_state
FOR EACH ROW
EXECUTE FUNCTION crowdrelay_set_updated_at();

INSERT INTO inventory_activation_state (workspace_id)
SELECT id FROM workspaces
ON CONFLICT (workspace_id) DO NOTHING;

CREATE TABLE inventory_stocktakes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    idempotency_key text NOT NULL CHECK (btrim(idempotency_key) <> '' AND char_length(idempotency_key) <= 128),
    request_hash bytea NOT NULL CHECK (octet_length(request_hash) = 32),
    actor_id text CHECK (actor_id IS NULL OR char_length(actor_id) <= 200),
    reason text CHECK (reason IS NULL OR char_length(reason) <= 500),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, idempotency_key)
);

CREATE TABLE inventory_stocktake_items (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    stocktake_id uuid NOT NULL,
    variant_id uuid NOT NULL,
    target_on_hand integer NOT NULL CHECK (target_on_hand >= 0),
    on_hand_before bigint NOT NULL,
    reserved_at_apply bigint NOT NULL CHECK (reserved_at_apply >= 0),
    applied_delta integer NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, stocktake_id, variant_id),
    CONSTRAINT inventory_stocktake_items_stocktake_fk FOREIGN KEY (workspace_id, stocktake_id)
        REFERENCES inventory_stocktakes (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT inventory_stocktake_items_variant_fk FOREIGN KEY (workspace_id, variant_id)
        REFERENCES merch_variants (workspace_id, id) ON DELETE RESTRICT,
    CHECK (on_hand_before + applied_delta = target_on_hand)
);

CREATE INDEX inventory_stocktakes_created_idx ON inventory_stocktakes (workspace_id, created_at DESC, id DESC);
CREATE INDEX inventory_stocktake_items_variant_idx ON inventory_stocktake_items (workspace_id, variant_id, created_at DESC);

WITH seed(slug, name, description, image_url, currency, price_gross_minor, active, public) AS (
    VALUES
        ('echoes', 'Echoes Of The Modern Mind', 'Nasz debiutancki album — 11 utworów przez szeroki umysł współczesnego człowieka. Każdy egzemplarz sygnowany przez zespół.', 'https://virya.music/images/merch/echoes.webp', 'PLN', 4000, true, true),
        ('ashes-color', 'From The Ashes — Koszulka Kolorowa', 'Nadruk all-over. Feniks powstaje w pełnym kolorze.', 'https://virya.music/images/merch/Ashes%20Color%20Front.webp', 'PLN', 5600, true, true),
        ('ashes-bw', 'From The Ashes — Koszulka monochrom', 'Nadruk all-over. Feniks w monochromatycznej odsłonie.', 'https://virya.music/images/merch/Ashes%20BW%20Front.webp', 'PLN', 5600, true, true),
        ('wave', 'Fala Niepewności — Koszulka', 'Nadruk all-over. Płyń na fali niepewności.', 'https://virya.music/images/merch/Wave%20Front.webp', 'PLN', 5600, true, true),
        ('virya-logo', 'Koszulka z Logo Viryi', 'Czysty srebrny herb z przodu, złoty emblemat z tyłu.', 'https://virya.music/images/merch/virya.webp', 'PLN', 4800, true, true),
        ('bag', 'Torba Viryi', 'Wytrzymała torba z herbem Viryi. Noś katharsis ze sobą.', 'https://virya.music/images/merch/bag1.webp', 'PLN', 4000, true, true)
)
INSERT INTO merch_products (workspace_id, slug, name, description, image_url, currency, price_gross_minor, active, public)
SELECT workspace.id, seed.slug, seed.name, seed.description, seed.image_url, seed.currency,
       seed.price_gross_minor, seed.active, seed.public
FROM workspaces AS workspace CROSS JOIN seed
ON CONFLICT (workspace_id, slug) DO NOTHING;

WITH seed(product_slug, sku, label, attributes, active, low_stock_threshold, sell_without_stock) AS (
    VALUES
        ('echoes', 'VIRYA-CD-ECHOES', 'Standard', '{}'::jsonb, true, 3, false),
        ('ashes-color', 'VIRYA-TEE-ASHES-COLOR-S', 'S', '{"size":"S"}'::jsonb, true, 2, false),
        ('ashes-color', 'VIRYA-TEE-ASHES-COLOR-M', 'M', '{"size":"M"}'::jsonb, true, 2, false),
        ('ashes-color', 'VIRYA-TEE-ASHES-COLOR-L', 'L', '{"size":"L"}'::jsonb, true, 2, false),
        ('ashes-color', 'VIRYA-TEE-ASHES-COLOR-XL', 'XL', '{"size":"XL"}'::jsonb, true, 2, false),
        ('ashes-color', 'VIRYA-TEE-ASHES-COLOR-XXL', 'XXL', '{"size":"XXL"}'::jsonb, true, 2, false),
        ('ashes-bw', 'VIRYA-TEE-ASHES-MONO-S', 'S', '{"size":"S"}'::jsonb, true, 2, false),
        ('ashes-bw', 'VIRYA-TEE-ASHES-MONO-M', 'M', '{"size":"M"}'::jsonb, true, 2, false),
        ('ashes-bw', 'VIRYA-TEE-ASHES-MONO-L', 'L', '{"size":"L"}'::jsonb, true, 2, false),
        ('ashes-bw', 'VIRYA-TEE-ASHES-MONO-XL', 'XL', '{"size":"XL"}'::jsonb, true, 2, false),
        ('ashes-bw', 'VIRYA-TEE-ASHES-MONO-XXL', 'XXL', '{"size":"XXL"}'::jsonb, true, 2, false),
        ('wave', 'VIRYA-TEE-WAVE-S', 'S', '{"size":"S"}'::jsonb, true, 2, false),
        ('wave', 'VIRYA-TEE-WAVE-M', 'M', '{"size":"M"}'::jsonb, true, 2, false),
        ('wave', 'VIRYA-TEE-WAVE-L', 'L', '{"size":"L"}'::jsonb, true, 2, false),
        ('wave', 'VIRYA-TEE-WAVE-XL', 'XL', '{"size":"XL"}'::jsonb, true, 2, false),
        ('wave', 'VIRYA-TEE-WAVE-XXL', 'XXL', '{"size":"XXL"}'::jsonb, true, 2, false),
        ('virya-logo', 'VIRYA-TEE-LOGO-S', 'S', '{"size":"S"}'::jsonb, true, 2, false),
        ('virya-logo', 'VIRYA-TEE-LOGO-M', 'M', '{"size":"M"}'::jsonb, true, 2, false),
        ('virya-logo', 'VIRYA-TEE-LOGO-L', 'L', '{"size":"L"}'::jsonb, true, 2, false),
        ('virya-logo', 'VIRYA-TEE-LOGO-XL', 'XL', '{"size":"XL"}'::jsonb, true, 2, false),
        ('virya-logo', 'VIRYA-TEE-LOGO-XXL', 'XXL', '{"size":"XXL"}'::jsonb, true, 2, false),
        ('bag', 'VIRYA-BAG-CREST', 'Standard', '{}'::jsonb, true, 3, false)
)
INSERT INTO merch_variants (workspace_id, product_id, sku, label, attributes, active, low_stock_threshold, sell_without_stock)
SELECT workspace.id, product.id, seed.sku, seed.label, seed.attributes, seed.active,
       seed.low_stock_threshold, seed.sell_without_stock
FROM workspaces AS workspace
CROSS JOIN seed
JOIN merch_products AS product ON product.workspace_id = workspace.id AND product.slug = seed.product_slug
ON CONFLICT (workspace_id, sku) DO NOTHING;

UPDATE inventory_activation_state
SET catalog_seeded_at = COALESCE(catalog_seeded_at, now()), catalog_seed_version = GREATEST(catalog_seed_version, 1)
WHERE EXISTS (SELECT 1 FROM merch_variants WHERE merch_variants.workspace_id = inventory_activation_state.workspace_id);
