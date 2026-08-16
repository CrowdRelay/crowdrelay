-- Physical-release fulfillment for existing Signal Latarnik / Beacon members.
--
-- Beacons remain the relationship source of truth. A release campaign snapshots
-- the currently active, contactable Latarnicy who explicitly keep the `releases`
-- topic enabled, reserves one real Commerce SKU for each of them, and only ships
-- to recipients who confirm delivery details for that campaign.

ALTER TABLE viryaos_beacon_signal_profiles
    ALTER COLUMN topics SET DEFAULT ARRAY['shows','press_materials','releases']::text[];

-- `releases` did not carry a physical-fulfillment meaning before this migration,
-- so existing members have not previously opted out of this newly introduced
-- topic. It remains removable through normal Latarnik preferences afterwards.
UPDATE viryaos_beacon_signal_profiles
SET topics = array_append(topics, 'releases'), updated_at = now()
WHERE NOT ('releases' = ANY(topics));

CREATE TABLE viryaos_beacon_release_campaigns (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    slug text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9_-]{0,127}$'),
    title text NOT NULL CHECK (btrim(title) <> '' AND char_length(title) <= 200),
    variant_id uuid NOT NULL,
    reservation_id uuid,
    status text NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft','open','closed','cancelled')),
    claim_deadline timestamptz NOT NULL,
    eligible_count integer NOT NULL DEFAULT 0 CHECK (eligible_count >= 0),
    reserved_quantity integer NOT NULL DEFAULT 0 CHECK (reserved_quantity >= 0),
    launched_at timestamptz,
    closed_at timestamptz,
    cancelled_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, id),
    UNIQUE (workspace_id, slug),
    CONSTRAINT viryaos_beacon_release_campaigns_variant_fk
        FOREIGN KEY (workspace_id, variant_id)
        REFERENCES merch_variants (workspace_id, id) ON DELETE RESTRICT,
    CONSTRAINT viryaos_beacon_release_campaigns_reservation_fk
        FOREIGN KEY (workspace_id, reservation_id)
        REFERENCES inventory_reservations (workspace_id, id) ON DELETE RESTRICT,
    CHECK (claim_deadline > created_at),
    CHECK ((status = 'draft') = (reservation_id IS NULL)),
    CHECK ((status = 'draft') = (launched_at IS NULL)),
    CHECK ((status = 'closed') = (closed_at IS NOT NULL)),
    CHECK ((status = 'cancelled') = (cancelled_at IS NOT NULL))
);
CREATE INDEX viryaos_beacon_release_campaigns_status_idx
    ON viryaos_beacon_release_campaigns (workspace_id, status, claim_deadline, id);
CREATE TRIGGER viryaos_beacon_release_campaigns_set_updated_at
BEFORE UPDATE ON viryaos_beacon_release_campaigns
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

CREATE TABLE viryaos_beacon_release_recipients (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    campaign_id uuid NOT NULL,
    beacon_id uuid NOT NULL,
    status text NOT NULL DEFAULT 'eligible'
        CHECK (status IN ('eligible','notified','confirmed','prepared','sent','delivered','declined','expired','cancelled')),
    recipient_name text CHECK (recipient_name IS NULL OR (btrim(recipient_name) <> '' AND char_length(recipient_name) <= 160)),
    recipient_phone text CHECK (recipient_phone IS NULL OR (btrim(recipient_phone) <> '' AND char_length(recipient_phone) <= 32)),
    parcel_locker_code text CHECK (parcel_locker_code IS NULL OR (btrim(parcel_locker_code) <> '' AND char_length(parcel_locker_code) <= 32)),
    notified_at timestamptz,
    confirmed_at timestamptz,
    prepared_at timestamptz,
    sent_at timestamptz,
    delivered_at timestamptz,
    declined_at timestamptz,
    expired_at timestamptz,
    cancelled_at timestamptz,
    delivery_details_purge_after timestamptz,
    pii_purged_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, campaign_id, beacon_id),
    CONSTRAINT viryaos_beacon_release_recipients_campaign_fk
        FOREIGN KEY (workspace_id, campaign_id)
        REFERENCES viryaos_beacon_release_campaigns (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT viryaos_beacon_release_recipients_beacon_fk
        FOREIGN KEY (workspace_id, beacon_id)
        REFERENCES viryaos_beacons (workspace_id, id) ON DELETE RESTRICT,
    CHECK (
        (status IN ('confirmed','prepared','sent') AND
         recipient_name IS NOT NULL AND recipient_phone IS NOT NULL AND parcel_locker_code IS NOT NULL)
        OR (status = 'delivered' AND (
            (recipient_name IS NOT NULL AND recipient_phone IS NOT NULL AND parcel_locker_code IS NOT NULL)
            OR pii_purged_at IS NOT NULL
        ))
        OR status NOT IN ('confirmed','prepared','sent','delivered')
    ),
    CHECK (pii_purged_at IS NULL OR (
        recipient_name IS NULL AND recipient_phone IS NULL AND parcel_locker_code IS NULL
    ))
);
CREATE INDEX viryaos_beacon_release_recipients_campaign_status_idx
    ON viryaos_beacon_release_recipients (workspace_id, campaign_id, status, beacon_id);
CREATE INDEX viryaos_beacon_release_recipients_pii_purge_idx
    ON viryaos_beacon_release_recipients (delivery_details_purge_after, workspace_id, campaign_id, beacon_id)
    WHERE delivery_details_purge_after IS NOT NULL AND pii_purged_at IS NULL;
CREATE TRIGGER viryaos_beacon_release_recipients_set_updated_at
BEFORE UPDATE ON viryaos_beacon_release_recipients
FOR EACH ROW EXECUTE FUNCTION crowdrelay_set_updated_at();

INSERT INTO viryaos_manager_config (workspace_id, config_key, value)
SELECT id, 'beacon_release_policy', jsonb_build_object(
    'copies_per_latarnik', 1,
    'delivery_confirmation_required', true,
    'delivery_details_retention_days_after_delivery', 30,
    'launch_requires_full_pool_stock', true,
    'default_country', 'PL'
) FROM workspaces
ON CONFLICT (workspace_id, config_key) DO NOTHING;
